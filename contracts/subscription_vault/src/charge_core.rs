//! Single charge logic (no auth). Used by charge_subscription and batch_charge.
//!
//! Charge runs only when status is Active or GracePeriod. On insufficient balance the
//! subscription is moved to a recoverable non-active state and an explicit failure
//! event is emitted without mutating financial accounting state.
//! On lifetime cap exhaustion the subscription is cancelled (terminal state).
//!
//! See `docs/subscription_lifecycle.md` for lifecycle details.
//! See `docs/lifetime_caps.md` for cap enforcement semantics.
//!
//! **PRs that only change how one subscription is charged should edit this file only.**

#![allow(dead_code)]

use crate::queries::get_subscription;
use crate::safe_math::{safe_add, safe_sub, safe_sub_balance};
use crate::state_machine::validate_status_transition;
use crate::statements::append_statement;
use crate::types::{
    BillingChargeKind, DataKey, Error, LifetimeCapReachedEvent, SubscriptionChargedEvent,
    SubscriptionStatus, UsageLimits, UsageState, UsageStatementEvent,
};
use soroban_sdk::{symbol_short, Env, String, Symbol};

const KEY_CHARGED_PERIOD: Symbol = symbol_short!("cp");
const KEY_IDEM: Symbol = symbol_short!("idem");

fn charged_period_key(subscription_id: u32) -> (Symbol, u32) {
    (KEY_CHARGED_PERIOD, subscription_id)
}

fn idem_key(subscription_id: u32) -> (Symbol, u32) {
    (KEY_IDEM, subscription_id)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChargeExecutionResult {
    Charged,
    InsufficientBalance,
}

/// Performs a single interval-based charge with optional replay protection.
pub fn charge_one(
    env: &Env,
    subscription_id: u32,
    now: u64,
    idempotency_key: Option<soroban_sdk::BytesN<32>>,
) -> Result<ChargeExecutionResult, Error> {
    let mut sub = get_subscription(env, subscription_id)?;
    let merchant = sub.merchant.clone();

    if crate::merchant::get_merchant_paused(env, merchant.clone()) {
        return Err(Error::MerchantPaused);
    }

    let charge_amount = crate::oracle::resolve_charge_amount(env, &sub)?;

    if sub.status != SubscriptionStatus::Active && sub.status != SubscriptionStatus::GracePeriod {
        return Err(Error::NotActive);
    }

    let period_index = now / sub.interval_seconds;

    // Idempotent return: same idempotency key already processed
    if let Some(ref k) = idempotency_key {
        if let Some(stored) = env
            .storage()
            .instance()
            .get::<_, soroban_sdk::BytesN<32>>(&idem_key(subscription_id))
        {
            if stored == *k {
                return Ok(ChargeExecutionResult::Charged);
            }
        }
    }

    // Replay: already charged for this billing period
    if let Some(stored_period) = env
        .storage()
        .instance()
        .get::<_, u64>(&charged_period_key(subscription_id))
    {
        if period_index <= stored_period {
            return Err(Error::Replay);
        }
    }

    let next_allowed = sub
        .last_payment_timestamp
        .checked_add(sub.interval_seconds)
        .ok_or(Error::Overflow)?;
    if now < next_allowed {
        return Err(Error::IntervalNotElapsed);
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    if let Some(cap) = sub.lifetime_cap {
        let remaining = safe_sub(cap, sub.lifetime_charged).unwrap_or(0).max(0);

        if remaining == 0 || charge_amount > remaining {
            // Cap already exhausted or this charge would exceed it — cancel.
            validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
            sub.status = SubscriptionStatus::Cancelled;
            env.storage().instance().set(&subscription_id, &sub);

            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );

            return Ok(ChargeExecutionResult::Charged);
        }
    }

    let storage = env.storage().instance();

    match safe_sub_balance(sub.prepaid_balance, charge_amount) {
        Ok(new_balance) => {
            sub.prepaid_balance = new_balance;
            crate::merchant::credit_merchant_balance_for_token(
                env,
                &sub.merchant,
                &sub.token,
                charge_amount,
                BillingChargeKind::Interval,
            )?;
            sub.last_payment_timestamp = now;

            sub.lifetime_charged = safe_add(sub.lifetime_charged, charge_amount)?;

            // Recover from grace period on successful charge
            if sub.status == SubscriptionStatus::GracePeriod {
                validate_status_transition(&sub.status, &SubscriptionStatus::Active)?;
                sub.status = SubscriptionStatus::Active;
                sub.grace_start_timestamp = None; // <-- CRITICAL FIX
            }

            // Check if cap is now exactly reached -- auto-cancel
            let cap_reached = sub
                .lifetime_cap
                .map(|cap| sub.lifetime_charged >= cap)
                .unwrap_or(false);

            if cap_reached {
                validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
                sub.status = SubscriptionStatus::Cancelled;
            }

            storage.set(&subscription_id, &sub);
            append_statement(
                env,
                subscription_id,
                charge_amount,
                sub.merchant.clone(),
                BillingChargeKind::Interval,
                next_allowed.saturating_sub(sub.interval_seconds),
                now,
            );

            // Record charged period and optional idempotency key
            storage.set(&charged_period_key(subscription_id), &period_index);
            if let Some(k) = idempotency_key {
                storage.set(&idem_key(subscription_id), &k);
            }

            env.events().publish(
                (symbol_short!("charged"),),
                SubscriptionChargedEvent {
                    subscription_id,
                    merchant: sub.merchant.clone(),
                    amount: charge_amount,
                    lifetime_charged: sub.lifetime_charged,
                },
            );

            if cap_reached {
                if let Some(cap) = sub.lifetime_cap {
                    env.events().publish(
                        (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                        LifetimeCapReachedEvent {
                            subscription_id,
                            lifetime_cap: cap,
                            lifetime_charged: sub.lifetime_charged,
                            timestamp: now,
                        },
                    );
                }
            }

            Ok(ChargeExecutionResult::Charged)
        }
        // charge_one.rs  —  replace the entire Err(_) arm in charge_one()
        Err(_) => {
            let grace_duration = crate::admin::get_grace_period(env).unwrap_or(0);
            let due_timestamp = sub
                .last_payment_timestamp
                .checked_add(sub.interval_seconds)
                .ok_or(Error::Overflow)?;

            let target_status = if grace_duration > 0 && now < grace_expires {
                SubscriptionStatus::GracePeriod
            } else {
                SubscriptionStatus::InsufficientBalance
            };

            if sub.status != target_status {
                validate_status_transition(&sub.status, &target_status)?;
                sub.status = target_status.clone();
            }

            storage.set(&subscription_id, &sub);

            let shortfall = charge_amount.saturating_sub(sub.prepaid_balance).max(0);
            env.events().publish(
                (Symbol::new(env, "charge_failed"), subscription_id),
                SubscriptionChargeFailedEvent {
                    subscription_id,
                    merchant: sub.merchant,
                    required_amount: charge_amount,
                    available_balance: sub.prepaid_balance,
                    shortfall,
                    resulting_status: target_status,
                    timestamp: now,
                },
            );

            Ok(ChargeExecutionResult::InsufficientBalance)
        }
    }
}

/// Debit a metered `usage_amount` from a subscription's prepaid balance.
pub fn charge_usage_one(
    env: &Env,
    subscription_id: u32,
    usage_amount: i128,
    reference: String,
) -> Result<(), Error> {
    let mut sub = get_subscription(env, subscription_id)?;
    let merchant = sub.merchant.clone();

    if crate::merchant::get_merchant_paused(env, merchant.clone()) {
        return Err(Error::MerchantPaused);
    }

    if sub.status != SubscriptionStatus::Active {
        return Err(Error::NotActive);
    }

    if !sub.usage_enabled {
        return Err(Error::UsageNotEnabled);
    }

    if usage_amount <= 0 {
        return Err(Error::InvalidAmount);
    }

    if sub.prepaid_balance < usage_amount {
        return Err(Error::InsufficientPrepaidBalance);
    }

    // -- Lifetime cap pre-check -----------------------------------------------
    if let Some(cap) = sub.lifetime_cap {
        let new_charged = safe_add(sub.lifetime_charged, usage_amount)?;
        if new_charged > cap {
            validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
            sub.status = SubscriptionStatus::Cancelled;
            env.storage().instance().set(&subscription_id, &sub);

            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );

            return Ok(());
        }
    }
    sub.lifetime_charged = new_charged;

    sub.prepaid_balance = safe_sub(sub.prepaid_balance, usage_amount)?;

    crate::merchant::credit_merchant_balance_for_token(
        env,
        &sub.merchant,
        &sub.token,
        usage_amount,
        BillingChargeKind::Usage,
    )?;

    if sub.prepaid_balance == 0 {
        validate_status_transition(&sub.status, &SubscriptionStatus::InsufficientBalance)?;
        sub.status = SubscriptionStatus::InsufficientBalance;
    }

    let cap_reached = sub
        .lifetime_cap
        .map(|cap| sub.lifetime_charged >= cap)
        .unwrap_or(false);

    if cap_reached {
        validate_status_transition(&sub.status, &SubscriptionStatus::Cancelled)?;
        sub.status = SubscriptionStatus::Cancelled;

        if let Some(cap) = sub.lifetime_cap {
            env.events().publish(
                (Symbol::new(env, "lifetime_cap_reached"), subscription_id),
                LifetimeCapReachedEvent {
                    subscription_id,
                    lifetime_cap: cap,
                    lifetime_charged: sub.lifetime_charged,
                    timestamp: now,
                },
            );
        }
    }

    storage.set(&subscription_id, &sub);
    storage.set(&ref_key, &reference);

    append_statement(
        env,
        subscription_id,
        usage_amount,
        sub.merchant.clone(),
        BillingChargeKind::Usage,
        now,
        now,
    );

    env.events().publish(
        (Symbol::new(env, "usage_statement"), subscription_id),
        UsageStatementEvent {
            subscription_id,
            merchant: sub.merchant.clone(),
            usage_amount,
            token: sub.token.clone(),
            timestamp: now,
            reference,
        },
    );

    Ok(())
}
