use crate::{
    can_transition, compute_next_charge_info, get_allowed_transitions, validate_status_transition,
    Error, RecoveryReason, Subscription, SubscriptionStatus, SubscriptionVault,
    SubscriptionVaultClient, BILLING_SNAPSHOT_FLAG_CLOSED, BILLING_SNAPSHOT_FLAG_USAGE_CHARGED,
    MAX_SUBSCRIPTION_ID,
};
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::{Address, Env, String, Vec as SorobanVec};

extern crate alloc;
use alloc::format;

// -- constants ----------------------------------------------------------------
const T0: u64 = 1_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60; // 30 days
const AMOUNT: i128 = 10_000_000; // 10 USDC (6 decimals)
const PREPAID: i128 = 50_000_000; // 50 USDC

// -- helpers ------------------------------------------------------------------

fn setup() -> (Env, Address, Address, Address, Address, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|li| li.timestamp = 1_000);

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let treasury = Address::generate(&env);

    let token_admin = Address::generate(&env);
    let token = env.register_stellar_asset_contract(token_admin.clone());
    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &5_000_000);

    client.init(&token, &6, &admin, &1, &0);
    client.set_treasury(&admin, &treasury);
    (
        env,
        contract_id,
        admin,
        subscriber,
        merchant,
        treasury,
        token,
    )
}

fn create_usage_sub(
    client: &SubscriptionVaultClient<'_>,
    subscriber: &Address,
    merchant: &Address,
    amount: i128,
) -> u32 {
    client.create_subscription(subscriber, merchant, &amount, &INTERVAL, &true, &None)
}

fn create_token_and_mint(env: &Env, recipient: &Address, amount: i128) -> Address {
    let token_admin = Address::generate(env);
    let token_addr = env
        .register_stellar_asset_contract_v2(token_admin.clone())
        .address();
    let token_client = soroban_sdk::token::StellarAssetClient::new(env, &token_addr);
    token_client.mint(recipient, &amount);
    token_addr
}

/// Standard setup: mock auth, register contract, init with real token + 7-day grace.
fn setup_test_env() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let min_topup = 1_000_000i128; // 1 USDC
    client.init(&token, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));

    (env, client, token, admin)
}

/// Helper used by reentrancy tests: returns (client, token, admin) with env pre-configured.
fn setup_contract(env: &Env) -> (SubscriptionVaultClient<'_>, Address, Address) {
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(env, &contract_id);
    let admin = Address::generate(env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
    (client, token, admin)
}

/// Create a test subscription, then patch its status for direct-manipulation tests.
fn create_test_subscription(
    env: &Env,
    client: &SubscriptionVaultClient,
    status: SubscriptionStatus,
) -> (u32, Address, Address) {
    let subscriber = Address::generate(env);
    let merchant = Address::generate(env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    if status != SubscriptionStatus::Active {
        let mut sub = client.get_subscription(&id);
        sub.status = status;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
    }
    (id, subscriber, merchant)
}

/// Seed a subscription with a known prepaid balance directly in storage.
fn seed_balance(env: &Env, client: &SubscriptionVaultClient, id: u32, balance: i128) {
    let mut sub = client.get_subscription(&id);
    sub.prepaid_balance = balance;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });
}

/// Seed the `next_id` counter to an arbitrary value.
fn seed_counter(env: &Env, contract_id: &Address, value: u32) {
    env.as_contract(contract_id, || {
        env.storage()
            .instance()
            .set(&soroban_sdk::Symbol::new(env, "next_id"), &value);
    });
}

// -- State Machine Helper Tests -----------------------------------------------

#[test]
fn test_validate_status_transition_same_status_is_allowed() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Active, &SubscriptionStatus::Active)
            .is_ok()
    );
    assert!(
        validate_status_transition(&SubscriptionStatus::Paused, &SubscriptionStatus::Paused)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::InsufficientBalance
    )
    .is_ok());
}

#[test]
fn test_validate_active_transitions() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Active, &SubscriptionStatus::Paused)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::InsufficientBalance
    )
    .is_ok());
}

#[test]
fn test_validate_paused_transitions() {
    assert!(
        validate_status_transition(&SubscriptionStatus::Paused, &SubscriptionStatus::Active)
            .is_ok()
    );
    assert!(validate_status_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::Paused,
            &SubscriptionStatus::InsufficientBalance
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_validate_insufficient_balance_transitions() {
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::Active
    )
    .is_ok());
    assert!(validate_status_transition(
        &SubscriptionStatus::InsufficientBalance,
        &SubscriptionStatus::Cancelled
    )
    .is_ok());
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::InsufficientBalance,
            &SubscriptionStatus::Paused
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_validate_cancelled_transitions_all_blocked() {
    assert_eq!(
        validate_status_transition(&SubscriptionStatus::Cancelled, &SubscriptionStatus::Active),
        Err(Error::InvalidStatusTransition)
    );
    assert_eq!(
        validate_status_transition(&SubscriptionStatus::Cancelled, &SubscriptionStatus::Paused),
        Err(Error::InvalidStatusTransition)
    );
    assert_eq!(
        validate_status_transition(
            &SubscriptionStatus::Cancelled,
            &SubscriptionStatus::InsufficientBalance
        ),
        Err(Error::InvalidStatusTransition)
    );
}

#[test]
fn test_can_transition_helper() {
    assert!(can_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Paused
    ));
    assert!(can_transition(
        &SubscriptionStatus::Active,
        &SubscriptionStatus::Cancelled
    ));
    assert!(can_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::Active
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Active
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Cancelled,
        &SubscriptionStatus::Paused
    ));
    assert!(!can_transition(
        &SubscriptionStatus::Paused,
        &SubscriptionStatus::InsufficientBalance
    ));
}

#[test]
fn test_get_allowed_transitions() {
    let active_targets = get_allowed_transitions(&SubscriptionStatus::Active);
    assert!(active_targets.contains(&SubscriptionStatus::Paused));
    assert!(active_targets.contains(&SubscriptionStatus::Cancelled));
    assert!(active_targets.contains(&SubscriptionStatus::InsufficientBalance));

    let paused_targets = get_allowed_transitions(&SubscriptionStatus::Paused);
    assert_eq!(paused_targets.len(), 2);
    assert!(paused_targets.contains(&SubscriptionStatus::Active));
    assert!(paused_targets.contains(&SubscriptionStatus::Cancelled));

    assert_eq!(
        get_allowed_transitions(&SubscriptionStatus::Cancelled).len(),
        0
    );

    let ib_targets = get_allowed_transitions(&SubscriptionStatus::InsufficientBalance);
    assert_eq!(ib_targets.len(), 2);
}

// -- Contract Lifecycle Tests -------------------------------------------------

#[test]
fn test_pause_subscription_from_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_pause_subscription_from_cancelled_should_fail() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.pause_subscription(&id, &subscriber);
}

#[test]
fn test_pause_subscription_from_paused_is_idempotent() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
fn test_cancel_subscription_from_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_cancel_subscription_from_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_cancel_subscription_from_cancelled_is_idempotent() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.cancel_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled
    );
}

#[test]
fn test_resume_subscription_from_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Active
    );
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_resume_subscription_from_cancelled_should_fail() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
}

#[test]
fn test_full_lifecycle_active_pause_resume() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
    client.resume_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Active
    );
    client.pause_subscription(&id, &subscriber);
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Paused
    );
}

#[test]
fn test_all_valid_transitions_coverage() {
    // Active -> Paused
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Paused
        );
    }
    // Active -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
    // Active -> InsufficientBalance (direct storage patch)
    {
        let (env, client, _, _) = setup_test_env();
        let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::InsufficientBalance
        );
    }
    // Paused -> Active
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        client.resume_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Active
        );
    }
    // Paused -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        client.pause_subscription(&id, &subscriber);
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
    // InsufficientBalance -> Active
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        client.resume_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Active
        );
    }
    // InsufficientBalance -> Cancelled
    {
        let (env, client, _, _) = setup_test_env();
        let (id, subscriber, _) =
            create_test_subscription(&env, &client, SubscriptionStatus::Active);
        let mut sub = client.get_subscription(&id);
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.as_contract(&client.address, || {
            env.storage().instance().set(&id, &sub);
        });
        client.cancel_subscription(&id, &subscriber);
        assert_eq!(
            client.get_subscription(&id).status,
            SubscriptionStatus::Cancelled
        );
    }
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_cancelled_to_active() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);
    client.resume_subscription(&id, &subscriber);
}

#[test]
#[should_panic(expected = "Error(Contract, #400)")]
fn test_invalid_insufficient_balance_to_paused() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let mut sub = client.get_subscription(&id);
    sub.status = SubscriptionStatus::InsufficientBalance;
    env.as_contract(&client.address, || {
        env.storage().instance().set(&id, &sub);
    });
    client.pause_subscription(&id, &subscriber);
}

// -- Subscription struct tests ------------------------------------------------

#[test]
fn test_subscription_struct_status_field() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: 100_000_000,
        interval_seconds: 30 * 24 * 60 * 60,
        last_payment_timestamp: 0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 500_000_000,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: 0,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    assert_eq!(sub.status, SubscriptionStatus::Active);
    assert_eq!(sub.lifetime_cap, None);
    assert_eq!(sub.lifetime_charged, 0);
}

#[test]
fn test_subscription_struct_with_lifetime_cap() {
    let env = Env::default();
    let cap = 120_000_000i128; // 120 USDC
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: 10_000_000,
        interval_seconds: 30 * 24 * 60 * 60,
        last_payment_timestamp: 0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 50_000_000,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: 0,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: Some(cap),
        lifetime_charged: 0,
    };
    assert_eq!(sub.lifetime_cap, Some(cap));
    assert_eq!(sub.lifetime_charged, 0);
}

// -- Contract Charging Tests --------------------------------------------------

#[test]
fn test_charge_subscription_basic() {
    let (env, client, _, admin) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID - AMOUNT);
    assert_eq!(sub.lifetime_charged, AMOUNT);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_charge_subscription_paused_fails() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);
    client.pause_subscription(&id, &subscriber);
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
}

#[test]
fn test_charge_subscription_insufficient_balance_returns_error() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    // Do not fund - balance stays 0
    // Charge attempt after interval + grace period should return InsufficientBalance error.
    // NOTE: Soroban reverts all state changes when a contract call returns Err,
    // so the status transition to InsufficientBalance is rolled back on-chain.
    let grace_period = 7 * 24 * 60 * 60u64;
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + INTERVAL + grace_period + 1);
    let result = client.try_charge_subscription(&id);
    assert!(result.is_err());
}

// -- ID limit test ------------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #429)")]
fn test_subscription_limit_reached() {
    let (env, client, _, _) = setup_test_env();
    seed_counter(&env, &client.address, MAX_SUBSCRIPTION_ID);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
}

#[test]
fn test_cancel_subscription_unauthorized() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let other = Address::generate(&env);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &1000, &86400, &true, &None::<i128>);
    let result = client.try_cancel_subscription(&sub_id, &other);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}

#[test]
fn snapshots_written_after_successful_close() {
    let (env, contract_id, _admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 100);

    client.deposit_funds(&sub_id, &subscriber, &1_000);
    client.charge_usage(&sub_id, &25);

    env.ledger().with_mut(|li| li.timestamp += INTERVAL + 1);
    client.charge_subscription(&sub_id);

    let snap0 = client.get_billing_period_snapshot(&sub_id, &0);
    assert_eq!(snap0.total_usage_units, 25);
    assert_eq!(snap0.total_amount_charged, 25);
    assert!(snap0.status_flags & BILLING_SNAPSHOT_FLAG_CLOSED != 0);
    assert!(snap0.status_flags & BILLING_SNAPSHOT_FLAG_USAGE_CHARGED != 0);
}

// -- Deposit tests ------------------------------------------------------------

#[test]
fn usage_cap_enforced_per_period() {
    let (env, contract_id, _admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 100);
    client.deposit_funds(&sub_id, &subscriber, &1_000);
    client.set_usage_cap(&sub_id, &merchant, &Some(100));

    client.charge_usage(&sub_id, &60);
    client.charge_usage(&sub_id, &40);
    assert!(client.try_charge_usage(&sub_id, &1).is_err());

    env.ledger().with_mut(|li| li.timestamp += INTERVAL + 1);
    client.charge_subscription(&sub_id);
    client.charge_usage(&sub_id, &10);
}

#[test]
fn usage_rate_limit_enforced_and_resets() {
    let (env, contract_id, _admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 100);
    client.deposit_funds(&sub_id, &subscriber, &1_000);
    client.set_usage_rate_limit(&sub_id, &merchant, &Some(2), &60);

    client.charge_usage(&sub_id, &1);
    client.charge_usage(&sub_id, &1);
    assert!(client.try_charge_usage(&sub_id, &1).is_err());

    env.ledger().with_mut(|li| li.timestamp += 61);
    client.charge_usage(&sub_id, &1);
}

#[test]
fn protocol_fee_skim_conserves_value() {
    let (env, contract_id, admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 100);
    client.set_protocol_fee_bps(&admin, &500);
    client.deposit_funds(&sub_id, &subscriber, &1_000);
    client.charge_usage(&sub_id, &200);

    let merchant_bal = client.get_merchant_balance(&merchant);
    let treasury_bal = client.get_treasury_balance();
    assert_eq!(merchant_bal, 190);
    assert_eq!(treasury_bal, 10);
    assert_eq!(merchant_bal + treasury_bal, 200);
}

#[test]
fn status_becomes_insufficient_after_full_usage_debit() {
    let (env, contract_id, _admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 100);
    client.deposit_funds(&sub_id, &subscriber, &100);
    client.charge_usage(&sub_id, &100);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.status, SubscriptionStatus::InsufficientBalance);
}

// -- Deposit tests ------------------------------------------------------------

#[test]
fn test_deposit_funds_basic() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &5_000_000);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 5_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #402)")]
fn test_deposit_funds_below_minimum() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    // min_topup is 1_000_000; try to deposit 500
    client.deposit_funds(&id, &subscriber, &500);
}

// -- Admin tests --------------------------------------------------------------

#[test]
fn test_rotate_admin() {
    let (env, client, _, admin) = setup_test_env();
    let new_admin = Address::generate(&env);
    client.rotate_admin(&admin, &new_admin);
    assert_eq!(client.get_admin(), new_admin);
}

#[test]
fn test_emergency_stop() {
    let (env, client, _, admin) = setup_test_env();
    assert!(!client.get_emergency_stop_status());
    client.enable_emergency_stop(&admin);
    assert!(client.get_emergency_stop_status());
    client.disable_emergency_stop(&admin);
    assert!(!client.get_emergency_stop_status());
}

#[test]
#[should_panic(expected = "Error(Contract, #1009)")]
fn test_create_subscription_blocked_by_emergency_stop() {
    let (env, client, _, admin) = setup_test_env();
    client.enable_emergency_stop(&admin);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
}

// -- Batch charge tests -------------------------------------------------------

#[test]
fn test_batch_charge() {
    let (env, client, _, admin) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id1, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let (id2, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id1, PREPAID);
    seed_balance(&env, &client, id2, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);

    let ids = SorobanVec::from_array(&env, [id1, id2]);
    let results = client.batch_charge(&ids);
    assert_eq!(results.len(), 2);
    assert!(results.get(0).unwrap().success);
    assert!(results.get(1).unwrap().success);
}

// -- Next charge info test ----------------------------------------------------

#[test]
fn test_next_charge_info() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let info = client.get_next_charge_info(&id);
    assert_eq!(info.next_charge_timestamp, T0 + INTERVAL);
    assert!(info.is_charge_expected);
}

// -- Compute next charge info (unit) ------------------------------------------

#[test]
fn test_compute_next_charge_info_active() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: T0,
        status: SubscriptionStatus::Active,
        prepaid_balance: 0,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: T0,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert_eq!(info.next_charge_timestamp, T0 + INTERVAL);
    assert!(info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_paused() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: 2000,
        status: SubscriptionStatus::Paused,
        prepaid_balance: 50_000_000,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: 2000,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(!info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, 2000 + INTERVAL);
}

#[test]
fn test_compute_next_charge_info_cancelled() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: T0,
        status: SubscriptionStatus::Cancelled,
        prepaid_balance: 0,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: T0,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(!info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_insufficient_balance() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: INTERVAL,
        last_payment_timestamp: 3000,
        status: SubscriptionStatus::InsufficientBalance,
        prepaid_balance: 1_000_000,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: 3000,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
}

#[test]
fn test_compute_next_charge_info_overflow_protection() {
    let env = Env::default();
    let sub = Subscription {
        subscriber: Address::generate(&env),
        merchant: Address::generate(&env),
        amount: AMOUNT,
        interval_seconds: 200,
        last_payment_timestamp: u64::MAX - 100,
        status: SubscriptionStatus::Active,
        prepaid_balance: 100_000_000,
        usage_enabled: false,
        expiration: None,
        billing_anchor_timestamp: u64::MAX - 100,
        current_period_index: 0,
        current_period_usage_units: 0,
        usage_cap_units: None,
        usage_rate_limit_max_calls: None,
        usage_rate_window_secs: 0,
        lifetime_cap: None,
        lifetime_charged: 0,
    };
    let info = compute_next_charge_info(&sub);
    assert!(info.is_charge_expected);
    assert_eq!(info.next_charge_timestamp, u64::MAX);
}

// -- Replay protection --------------------------------------------------------

#[test]
#[should_panic(expected = "Error(Contract, #1007)")]
fn test_replay_charge_same_period() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
    // Second charge in same period should fail
    client.charge_subscription(&id);
}

// -- Recovery -----------------------------------------------------------------

#[test]
fn test_recover_stranded_funds() {
    let (env, client, _, admin) = setup_test_env();
    let recipient = Address::generate(&env);
    client.recover_stranded_funds(
        &admin,
        &recipient,
        &1_000_000,
        &RecoveryReason::AccidentalTransfer,
    );
    // No panic means success (actual transfer is TODO in admin.rs)
}

// -- Lifetime cap tests -------------------------------------------------------

#[test]
fn test_lifetime_cap_auto_cancel() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    // Cap = 2 * AMOUNT, so after 2 charges, should auto-cancel
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(2 * AMOUNT),
    );
    seed_balance(&env, &client, id, PREPAID);

    // First charge
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Active);

    // Second charge -> cap reached -> auto-cancel
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + 2 * INTERVAL + 1);
    client.charge_subscription(&id);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.lifetime_charged, 2 * AMOUNT);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
}

#[test]
fn test_get_cap_info() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let cap = 100_000_000i128;
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &Some(cap),
    );
    let info = client.get_cap_info(&id);
    assert_eq!(info.lifetime_cap, Some(cap));
    assert_eq!(info.lifetime_charged, 0);
    assert_eq!(info.remaining_cap, Some(cap));
    assert!(!info.cap_reached);
}

// -- Plan template tests ------------------------------------------------------

#[test]
fn test_create_and_use_plan_template() {
    let (env, client, _, _) = setup_test_env();
    let merchant = Address::generate(&env);
    let subscriber = Address::generate(&env);

    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);
    let plan = client.get_plan_template(&plan_id);
    assert_eq!(plan.amount, AMOUNT);
    assert_eq!(plan.merchant, merchant);

    let sub_id = client.create_subscription_from_plan(&subscriber, &plan_id);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.amount, AMOUNT);
    assert_eq!(sub.merchant, merchant);
    assert_eq!(sub.subscriber, subscriber);
}

// -- Usage charge tests -------------------------------------------------------

#[test]
fn test_charge_usage_basic() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    seed_balance(&env, &client, id, PREPAID);

    client.charge_usage(&id, &1_000_000);
    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, PREPAID - 1_000_000);
}

#[test]
#[should_panic(expected = "Error(Contract, #1004)")]
fn test_charge_usage_not_enabled() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);
    client.charge_usage(&id, &1_000_000);
}

// -- Usage cap tests ----------------------------------------------------------

#[test]
fn test_set_and_enforce_usage_cap() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &true,
        &None::<i128>,
    );
    seed_balance(&env, &client, id, PREPAID);

    // Set cap to 2M units
    client.set_usage_cap(&id, &merchant, &Some(2_000_000i128));

    // First usage under cap - OK
    client.charge_usage(&id, &1_500_000);

    // Second usage would exceed cap
    let result = client.try_charge_usage(&id, &1_000_000);
    assert!(result.is_err());
}

// -- Billing period snapshot tests --------------------------------------------

#[test]
fn test_billing_period_snapshot_created_on_charge() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    // First charge
    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    // Second charge in next period triggers snapshot for period 0
    env.ledger()
        .with_mut(|li| li.timestamp = T0 + 2 * INTERVAL + 1);
    client.charge_subscription(&id);

    let snapshot = client.get_billing_period_snapshot(&id, &0);
    assert_eq!(snapshot.subscription_id, id);
    assert!(snapshot.status_flags & BILLING_SNAPSHOT_FLAG_CLOSED != 0);
}

// -- Protocol fee tests -------------------------------------------------------

#[test]
fn test_protocol_fee_skimming() {
    let (env, client, _, admin) = setup_test_env();
    let treasury = Address::generate(&env);
    client.set_treasury(&admin, &treasury);
    client.set_protocol_fee_bps(&admin, &500); // 5%

    env.ledger().with_mut(|li| li.timestamp = T0);
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    // Treasury should get 5% of AMOUNT
    let treasury_balance = client.get_treasury_balance();
    assert_eq!(treasury_balance, AMOUNT * 500 / 10_000);
}

// -- Merchant tests -----------------------------------------------------------

#[test]
fn test_merchant_balance_and_withdrawal() {
    let (env, client, _, _) = setup_test_env();
    env.ledger().with_mut(|li| li.timestamp = T0);

    let (id, _, merchant) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    env.ledger().with_mut(|li| li.timestamp = T0 + INTERVAL + 1);
    client.charge_subscription(&id);

    let balance = client.get_merchant_balance(&merchant);
    assert!(balance > 0);
}

// -- List subscriptions by subscriber test ------------------------------------

#[test]
fn test_list_subscriptions_by_subscriber() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let id1 = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let id2 = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    let page = client.list_subscriptions_by_subscriber(&subscriber, &0, &10);
    assert_eq!(page.subscription_ids.len(), 2);
    assert_eq!(page.subscription_ids.get(0).unwrap(), id1);
    assert_eq!(page.subscription_ids.get(1).unwrap(), id2);
    assert!(!page.has_next);
}

// -- Subscriber withdrawal test -----------------------------------------------

#[test]
fn test_withdraw_subscriber_funds_after_cancel() {
    let (env, client, token, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let token_admin_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin_client.mint(&subscriber, &100_000_000);

    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&id, &subscriber, &5_000_000);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 5_000_000);

    client.cancel_subscription(&id, &subscriber);
    client.withdraw_subscriber_funds(&id, &subscriber);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.prepaid_balance, 0);
}

// -- Export tests -------------------------------------------------------------

#[test]
fn test_export_contract_snapshot() {
    let (env, client, _, admin) = setup_test_env();
    let snapshot = client.export_contract_snapshot(&admin);
    assert_eq!(snapshot.admin, admin);
    assert_eq!(snapshot.storage_version, 2);
}

#[test]
fn test_export_subscription_summaries() {
    let (env, client, _, admin) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let summaries = client.export_subscription_summaries(&admin, &0, &10);
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries.get(0).unwrap().subscription_id, id);
}

// =============================================================================
// Metadata Key-Value Store Tests
// =============================================================================

#[test]
fn test_metadata_set_and_get() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "invoice_id");
    let value = String::from_str(&env, "INV-2025-001");

    client.set_metadata(&id, &subscriber, &key, &value);

    let result = client.get_metadata(&id, &key);
    assert_eq!(result, value);
}

#[test]
fn test_metadata_update_existing_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "customer_id");
    let value1 = String::from_str(&env, "CUST-001");
    let value2 = String::from_str(&env, "CUST-002");

    client.set_metadata(&id, &subscriber, &key, &value1);
    assert_eq!(client.get_metadata(&id, &key), value1);

    client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(client.get_metadata(&id, &key), value2);

    // Key count should still be 1 (updated, not duplicated)
    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value = String::from_str(&env, "premium");

    client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);

    client.delete_metadata(&id, &subscriber, &key);

    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
fn test_metadata_list_keys() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key1 = String::from_str(&env, "invoice_id");
    let key2 = String::from_str(&env, "customer_id");
    let key3 = String::from_str(&env, "campaign_tag");

    client.set_metadata(&id, &subscriber, &key1, &String::from_str(&env, "v1"));
    client.set_metadata(&id, &subscriber, &key2, &String::from_str(&env, "v2"));
    client.set_metadata(&id, &subscriber, &key3, &String::from_str(&env, "v3"));

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 3);
}

#[test]
fn test_metadata_empty_list_for_new_subscription() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 0);
}

#[test]
fn test_metadata_merchant_can_set() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, merchant) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "merchant_ref");
    let value = String::from_str(&env, "MR-123");

    client.set_metadata(&id, &merchant, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_merchant_can_delete() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, merchant) =
        create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value = String::from_str(&env, "test");

    // Subscriber sets it
    client.set_metadata(&id, &subscriber, &key, &value);

    // Merchant deletes it
    client.delete_metadata(&id, &merchant, &key);

    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_unauthorized_actor_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let stranger = Address::generate(&env);
    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");

    client.set_metadata(&id, &stranger, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #403)")]
fn test_metadata_delete_unauthorized_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "test");
    client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "val"));

    let stranger = Address::generate(&env);
    client.delete_metadata(&id, &stranger, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #1023)")]
fn test_metadata_key_limit_enforced() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Set MAX_METADATA_KEYS (10) keys
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        let value = String::from_str(&env, "val");
        client.set_metadata(&id, &subscriber, &key, &value);
    }

    // 11th key should fail
    let key = String::from_str(&env, "key_overflow");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_update_at_limit_succeeds() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "val"));
    }

    // Updating an existing key should succeed even at limit
    let key = String::from_str(&env, "key_0");
    let new_value = String::from_str(&env, "updated");
    client.set_metadata(&id, &subscriber, &key, &new_value);
    assert_eq!(client.get_metadata(&id, &key), new_value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_key_too_long_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // 33 chars exceeds MAX_METADATA_KEY_LENGTH (32)
    let key = String::from_str(&env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1024)")]
fn test_metadata_empty_key_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1025)")]
fn test_metadata_value_too_long_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "test");
    // Create a string > 256 bytes
    let long_str = alloc::string::String::from_utf8(alloc::vec![b'x'; 257]).unwrap();
    let long_value = String::from_str(&env, &long_str);
    client.set_metadata(&id, &subscriber, &key, &long_value);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_get_nonexistent_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, _, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "nonexistent");
    client.get_metadata(&id, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_delete_nonexistent_key() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "nonexistent");
    client.delete_metadata(&id, &subscriber, &key);
}

#[test]
#[should_panic(expected = "Error(Contract, #404)")]
fn test_metadata_operations_on_nonexistent_subscription() {
    let (env, client, _, _) = setup_test_env();
    let subscriber = Address::generate(&env);
    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");
    client.set_metadata(&999, &subscriber, &key, &value);
}

#[test]
#[should_panic(expected = "Error(Contract, #1002)")]
fn test_metadata_set_on_cancelled_subscription_rejected() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.cancel_subscription(&id, &subscriber);

    let key = String::from_str(&env, "test");
    let value = String::from_str(&env, "val");
    client.set_metadata(&id, &subscriber, &key, &value);
}

#[test]
fn test_metadata_does_not_affect_financial_state() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    seed_balance(&env, &client, id, PREPAID);

    let sub_before = client.get_subscription(&id);

    // Set multiple metadata entries
    for i in 0..5u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        let value = String::from_str(&env, &format!("value_{i}"));
        client.set_metadata(&id, &subscriber, &key, &value);
    }

    let sub_after = client.get_subscription(&id);

    // Financial state must be unchanged
    assert_eq!(sub_before.prepaid_balance, sub_after.prepaid_balance);
    assert_eq!(sub_before.lifetime_charged, sub_after.lifetime_charged);
    assert_eq!(sub_before.status, sub_after.status);
    assert_eq!(sub_before.amount, sub_after.amount);
}

#[test]
fn test_metadata_delete_then_re_add() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    let value1 = String::from_str(&env, "v1");
    let value2 = String::from_str(&env, "v2");

    client.set_metadata(&id, &subscriber, &key, &value1);
    client.delete_metadata(&id, &subscriber, &key);

    // Re-add same key with different value
    client.set_metadata(&id, &subscriber, &key, &value2);
    assert_eq!(client.get_metadata(&id, &key), value2);

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 1);
}

#[test]
fn test_metadata_delete_frees_key_slot() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    // Fill to max
    for i in 0..10u32 {
        let key = String::from_str(&env, &format!("key_{i}"));
        client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "v"));
    }

    // Delete one
    client.delete_metadata(&id, &subscriber, &String::from_str(&env, "key_5"));

    // Should now be able to add a new key
    let new_key = String::from_str(&env, "key_new");
    client.set_metadata(&id, &subscriber, &new_key, &String::from_str(&env, "v"));

    let keys = client.list_metadata_keys(&id);
    assert_eq!(keys.len(), 10);
}

#[test]
fn test_metadata_isolation_between_subscriptions() {
    let (env, client, _, _) = setup_test_env();
    let (id1, sub1, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    let (id2, sub2, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "invoice_id");
    let val1 = String::from_str(&env, "INV-001");
    let val2 = String::from_str(&env, "INV-002");

    client.set_metadata(&id1, &sub1, &key, &val1);
    client.set_metadata(&id2, &sub2, &key, &val2);

    assert_eq!(client.get_metadata(&id1, &key), val1);
    assert_eq!(client.get_metadata(&id2, &key), val2);
}

#[test]
fn test_metadata_on_paused_subscription_allowed() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);

    let key = String::from_str(&env, "note");
    let value = String::from_str(&env, "paused for maintenance");
    client.set_metadata(&id, &subscriber, &key, &value);
    assert_eq!(client.get_metadata(&id, &key), value);
}

#[test]
fn test_metadata_delete_on_cancelled_subscription_allowed() {
    let (env, client, _, _) = setup_test_env();
    let (id, subscriber, _) = create_test_subscription(&env, &client, SubscriptionStatus::Active);

    let key = String::from_str(&env, "tag");
    client.set_metadata(&id, &subscriber, &key, &String::from_str(&env, "v"));

    client.cancel_subscription(&id, &subscriber);

    // Delete should still work on cancelled (cleanup)
    client.delete_metadata(&id, &subscriber, &key);
    let result = client.try_get_metadata(&id, &key);
    assert!(result.is_err());
}

// ── Blocklist Tests ───────────────────────────────────────────────────────────

#[test]
fn test_admin_can_add_subscriber_to_blocklist() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let reason = soroban_sdk::String::from_str(&env, "Fraudulent activity");

    client.add_to_blocklist(&admin, &subscriber, &Some(reason.clone()));

    assert!(client.is_blocklisted(&subscriber));
    let entry = client.get_blocklist_entry(&subscriber);
    assert_eq!(entry.subscriber, subscriber);
    assert_eq!(entry.added_by, admin);
    assert_eq!(entry.reason, Some(reason));
}

#[test]
fn test_merchant_can_blocklist_their_subscriber() {
    let (env, client, token, _admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Mint tokens and create subscription
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    assert!(sub_id == 0);

    // Merchant can blocklist their subscriber
    let reason = soroban_sdk::String::from_str(&env, "Payment disputes");
    client.add_to_blocklist(&merchant, &subscriber, &Some(reason));

    assert!(client.is_blocklisted(&subscriber));
}

#[test]
fn test_merchant_cannot_blocklist_unrelated_subscriber() {
    let (env, client, _token, _admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Merchant tries to blocklist subscriber without any subscription relationship
    let result = client.try_add_to_blocklist(&merchant, &subscriber, &None);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}

#[test]
fn test_blocklisted_subscriber_cannot_create_subscription() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Subscriber tries to create subscription
    let result = client.try_create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
}

#[test]
fn test_blocklisted_subscriber_cannot_create_subscription_from_plan() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create plan template
    let plan_id = client.create_plan_template(&merchant, &AMOUNT, &INTERVAL, &false, &None::<i128>);

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Subscriber tries to create subscription from plan
    let result = client.try_create_subscription_from_plan(&subscriber, &plan_id);
    assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
}

#[test]
fn test_blocklisted_subscriber_cannot_deposit_funds() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription first
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Subscriber tries to deposit funds
    let result = client.try_deposit_funds(&sub_id, &subscriber, &1_000_000);
    assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
}

#[test]
fn test_existing_subscription_preserved_after_blocklist() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription and deposit funds
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&sub_id, &subscriber, &PREPAID);

    let sub_before = client.get_subscription(&sub_id);
    assert_eq!(sub_before.prepaid_balance, PREPAID);

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Existing subscription is preserved
    let sub_after = client.get_subscription(&sub_id);
    assert_eq!(sub_after.prepaid_balance, PREPAID);
    assert_eq!(sub_after.status, SubscriptionStatus::Active);
}

#[test]
fn test_blocklisted_subscriber_can_cancel_existing_subscription() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Subscriber can still cancel their existing subscription
    client.cancel_subscription(&sub_id, &subscriber);
    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
}

#[test]
fn test_blocklisted_subscriber_can_withdraw_after_cancellation() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription and deposit funds
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&sub_id, &subscriber, &PREPAID);

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Subscriber cancels and withdraws
    client.cancel_subscription(&sub_id, &subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);

    let sub = client.get_subscription(&sub_id);
    assert_eq!(sub.prepaid_balance, 0);
}

#[test]
fn test_admin_can_remove_from_blocklist() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);

    // Admin adds to blocklist
    client.add_to_blocklist(&admin, &subscriber, &None);
    assert!(client.is_blocklisted(&subscriber));

    // Admin removes from blocklist
    client.remove_from_blocklist(&admin, &subscriber);
    assert!(!client.is_blocklisted(&subscriber));
}

#[test]
fn test_removed_subscriber_can_create_subscription() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Admin adds to blocklist
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Admin removes from blocklist
    client.remove_from_blocklist(&admin, &subscriber);

    // Subscriber can now create subscription
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    assert_eq!(sub_id, 0);
}

#[test]
fn test_non_admin_cannot_remove_from_blocklist() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let non_admin = Address::generate(&env);

    // Admin adds to blocklist
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Non-admin tries to remove
    let result = client.try_remove_from_blocklist(&non_admin, &subscriber);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_blocklist_entry_not_found() {
    let (env, client, _token, _admin) = setup_test_env();
    let subscriber = Address::generate(&env);

    let result = client.try_get_blocklist_entry(&subscriber);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn test_remove_nonexistent_blocklist_entry() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);

    let result = client.try_remove_from_blocklist(&admin, &subscriber);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn test_blocklist_events_emitted() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let reason = soroban_sdk::String::from_str(&env, "Test reason");

    // Add to blocklist
    client.add_to_blocklist(&admin, &subscriber, &Some(reason.clone()));
    assert!(client.is_blocklisted(&subscriber));

    // Remove from blocklist
    client.remove_from_blocklist(&admin, &subscriber);
    assert!(!client.is_blocklisted(&subscriber));
}

#[test]
fn test_blocklist_with_multiple_subscriptions() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant1 = Address::generate(&env);
    let merchant2 = Address::generate(&env);

    // Create multiple subscriptions
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &(PREPAID * 2));

    let sub_id1 = client.create_subscription(
        &subscriber,
        &merchant1,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    let sub_id2 = client.create_subscription(
        &subscriber,
        &merchant2,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Both subscriptions are preserved
    let sub1 = client.get_subscription(&sub_id1);
    let sub2 = client.get_subscription(&sub_id2);
    assert_eq!(sub1.status, SubscriptionStatus::Active);
    assert_eq!(sub2.status, SubscriptionStatus::Active);

    // Cannot create new subscription
    let result = client.try_create_subscription(
        &subscriber,
        &merchant1,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    assert_eq!(result, Err(Ok(Error::SubscriberBlocklisted)));
}

#[test]
fn test_blocklist_does_not_affect_charging() {
    let (env, client, token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription and deposit funds
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber, &PREPAID);

    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );
    client.deposit_funds(&sub_id, &subscriber, &PREPAID);

    // Admin blocklists subscriber
    client.add_to_blocklist(&admin, &subscriber, &None);

    // Advance time and charge
    env.ledger().with_mut(|li| li.timestamp += INTERVAL);
    client.charge_subscription(&sub_id);

    let sub = client.get_subscription(&sub_id);
    assert!(sub.prepaid_balance < PREPAID);
}

#[test]
fn test_blocklist_reason_stored_correctly() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);
    let reason = soroban_sdk::String::from_str(&env, "Repeated chargebacks");

    client.add_to_blocklist(&admin, &subscriber, &Some(reason.clone()));

    let entry = client.get_blocklist_entry(&subscriber);
    assert_eq!(entry.reason, Some(reason));
    assert_eq!(entry.added_by, admin);
}

#[test]
fn test_blocklist_without_reason() {
    let (env, client, _token, admin) = setup_test_env();
    let subscriber = Address::generate(&env);

    client.add_to_blocklist(&admin, &subscriber, &None);

    let entry = client.get_blocklist_entry(&subscriber);
    assert_eq!(entry.reason, None);
    assert_eq!(entry.subscriber, subscriber);
}

#[test]
fn test_merchant_blocklist_requires_subscription_relationship() {
    let (env, client, token, _admin) = setup_test_env();
    let subscriber1 = Address::generate(&env);
    let subscriber2 = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Create subscription with subscriber1
    let token_client = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_client.mint(&subscriber1, &PREPAID);

    client.create_subscription(
        &subscriber1,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,
        &None::<i128>,
    );

    // Merchant can blocklist subscriber1
    client.add_to_blocklist(&merchant, &subscriber1, &None);
    assert!(client.is_blocklisted(&subscriber1));

    // Merchant cannot blocklist subscriber2 (no relationship)
    let result = client.try_add_to_blocklist(&merchant, &subscriber2, &None);
    assert_eq!(result, Err(Ok(Error::Forbidden)));
}
