use crate::{
    SubscriptionStatus, SubscriptionVault, SubscriptionVaultClient,
    BILLING_SNAPSHOT_FLAG_CLOSED, BILLING_SNAPSHOT_FLAG_USAGE_CHARGED,
};
use soroban_sdk::testutils::{Address as _, Ledger as _};
use soroban_sdk::{Address, Env};

const INTERVAL: u64 = 10;

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
    (env, contract_id, admin, subscriber, merchant, treasury, token)
}

fn create_usage_sub(
    client: &SubscriptionVaultClient<'_>,
    subscriber: &Address,
    merchant: &Address,
    amount: i128,
) -> u32 {
    client.create_subscription(subscriber, merchant, &amount, &INTERVAL, &true, &None)
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

#[test]
fn failed_charge_does_not_create_snapshot() {
    let (env, contract_id, _admin, subscriber, merchant, _treasury, _token) = setup();
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let sub_id = create_usage_sub(&client, &subscriber, &merchant, 500);

    env.ledger().with_mut(|li| li.timestamp += INTERVAL + 1);
    assert!(client.try_charge_subscription(&sub_id).is_err());
    assert!(client.try_get_billing_period_snapshot(&sub_id, &0).is_err());
}

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
