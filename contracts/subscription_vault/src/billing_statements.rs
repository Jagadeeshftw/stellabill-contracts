use soroban_sdk::{symbol_short, Address, Env, Vec};

use crate::types::{
    BillingStatement, BillingStatementFinalization, BillingStatementPersistedEvent,
    BillingStatementRef, DataKey, Error,
};

fn to_ref(statement: &BillingStatement) -> BillingStatementRef {
    BillingStatementRef {
        subscription_id: statement.subscription_id,
        period_index: statement.period_index,
        period_end_timestamp: statement.period_end_timestamp,
    }
}

fn contains_ref(items: &Vec<BillingStatementRef>, target: &BillingStatementRef) -> bool {
    let mut i = 0;
    while i < items.len() {
        let item = items.get(i).unwrap();
        if item.subscription_id == target.subscription_id && item.period_index == target.period_index
        {
            return true;
        }
        i += 1;
    }
    false
}

pub fn upsert_statement(env: &Env, statement: BillingStatement) {
    let statement_key = DataKey::BillingStatement(statement.subscription_id, statement.period_index);
    env.storage().instance().set(&statement_key, &statement);

    let statement_ref = to_ref(&statement);

    let sub_index_key = DataKey::BillingStatementsBySubscription(statement.subscription_id);
    let mut sub_refs: Vec<BillingStatementRef> = env
        .storage()
        .instance()
        .get(&sub_index_key)
        .unwrap_or(Vec::new(env));
    if !contains_ref(&sub_refs, &statement_ref) {
        sub_refs.push_back(statement_ref.clone());
        env.storage().instance().set(&sub_index_key, &sub_refs);
    }

    let merchant_index_key = DataKey::BillingStatementsByMerchant(statement.merchant.clone());
    let mut merchant_refs: Vec<BillingStatementRef> = env
        .storage()
        .instance()
        .get(&merchant_index_key)
        .unwrap_or(Vec::new(env));
    if !contains_ref(&merchant_refs, &statement_ref) {
        merchant_refs.push_back(statement_ref);
        env.storage().instance().set(&merchant_index_key, &merchant_refs);
    }

    env.events().publish(
        (symbol_short!("bill_stmt"),),
        BillingStatementPersistedEvent {
            subscription_id: statement.subscription_id,
            period_index: statement.period_index,
            merchant: statement.merchant,
            finalized_by: statement.finalized_by,
        },
    );
}

pub fn get_statement(env: &Env, subscription_id: u32, period_index: u32) -> Result<BillingStatement, Error> {
    env.storage()
        .instance()
        .get(&DataKey::BillingStatement(subscription_id, period_index))
        .ok_or(Error::NotFound)
}

pub fn list_statements_by_subscription(
    env: &Env,
    subscription_id: u32,
    start: u32,
    limit: u32,
) -> Vec<BillingStatement> {
    let index_key = DataKey::BillingStatementsBySubscription(subscription_id);
    let refs: Vec<BillingStatementRef> = env.storage().instance().get(&index_key).unwrap_or(Vec::new(env));
    if limit == 0 || start >= refs.len() {
        return Vec::new(env);
    }

    let end = if start + limit > refs.len() {
        refs.len()
    } else {
        start + limit
    };

    let mut out = Vec::new(env);
    let mut i = start;
    while i < end {
        let r = refs.get(i).unwrap();
        if let Some(statement) = env
            .storage()
            .instance()
            .get::<_, BillingStatement>(&DataKey::BillingStatement(r.subscription_id, r.period_index))
        {
            out.push_back(statement);
        }
        i += 1;
    }
    out
}

pub fn list_statements_by_merchant_time_range(
    env: &Env,
    merchant: Address,
    start_timestamp: u64,
    end_timestamp: u64,
    start: u32,
    limit: u32,
) -> Vec<BillingStatement> {
    let index_key = DataKey::BillingStatementsByMerchant(merchant);
    let refs: Vec<BillingStatementRef> = env.storage().instance().get(&index_key).unwrap_or(Vec::new(env));
    if limit == 0 {
        return Vec::new(env);
    }

    let mut filtered = Vec::new(env);
    let mut i = 0;
    while i < refs.len() {
        let r = refs.get(i).unwrap();
        if r.period_end_timestamp >= start_timestamp && r.period_end_timestamp <= end_timestamp {
            filtered.push_back(r);
        }
        i += 1;
    }

    if start >= filtered.len() {
        return Vec::new(env);
    }

    let end = if start + limit > filtered.len() {
        filtered.len()
    } else {
        start + limit
    };

    let mut out = Vec::new(env);
    let mut j = start;
    while j < end {
        let r = filtered.get(j).unwrap();
        if let Some(statement) = env
            .storage()
            .instance()
            .get::<_, BillingStatement>(&DataKey::BillingStatement(r.subscription_id, r.period_index))
        {
            out.push_back(statement);
        }
        j += 1;
    }
    out
}

pub fn build_statement(
    env: &Env,
    subscription_id: u32,
    period_index: u32,
    merchant: Address,
    subscriber: Address,
    period_start_timestamp: u64,
    period_end_timestamp: u64,
    total_amount_charged: i128,
    total_usage_units: i128,
    protocol_fee_amount: i128,
    net_amount_to_merchant: i128,
    refund_amount: i128,
    status_flags: u32,
    subscription_status: crate::types::SubscriptionStatus,
    finalized_by: BillingStatementFinalization,
    finalized_at: u64,
) -> Result<BillingStatement, Error> {
    let token: Address = env
        .storage()
        .instance()
        .get(&soroban_sdk::Symbol::new(env, "token"))
        .ok_or(Error::NotInitialized)?;

    Ok(BillingStatement {
        subscription_id,
        period_index,
        snapshot_period_index: period_index,
        merchant,
        subscriber,
        token,
        period_start_timestamp,
        period_end_timestamp,
        total_amount_charged,
        total_usage_units,
        protocol_fee_amount,
        net_amount_to_merchant,
        refund_amount,
        status_flags,
        subscription_status,
        finalized_by,
        finalized_at,
    })
}
