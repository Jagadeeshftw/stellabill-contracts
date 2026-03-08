# Billing Statements (Past Periods)

## Purpose

`BillingStatement` persists a compact per-period billing record optimized for Past Periods UX.
It is written when a period is closed, on cancellation, and on final settlement.

## Schema

`BillingStatement` fields:

- `subscription_id: u32`
- `period_index: u32`
- `snapshot_period_index: u32` (reference to billing snapshot period)
- `merchant: Address`
- `subscriber: Address`
- `token: Address`
- `period_start_timestamp: u64`
- `period_end_timestamp: u64`
- `total_amount_charged: i128`
- `total_usage_units: i128`
- `protocol_fee_amount: i128`
- `net_amount_to_merchant: i128`
- `refund_amount: i128`
- `status_flags: u32`
- `subscription_status: SubscriptionStatus`
- `finalized_by: BillingStatementFinalization` (`PeriodClosed`, `Cancellation`, `FinalSettlement`)
- `finalized_at: u64`

## Storage and indexing

Primary record:

- `DataKey::BillingStatement(subscription_id, period_index) -> BillingStatement`

Secondary indices:

- `DataKey::BillingStatementsBySubscription(subscription_id) -> Vec<BillingStatementRef>`
- `DataKey::BillingStatementsByMerchant(merchant) -> Vec<BillingStatementRef>`

`BillingStatementRef`:

- `subscription_id`
- `period_index`
- `period_end_timestamp`

This avoids scanning all contract state for subscription and merchant Past Periods queries.

## Query entrypoints

- `get_billing_statement(subscription_id, period_index)`
- `get_bill_stmts_by_sub(subscription_id, start, limit)`
- `get_bill_stmts_by_merch_rng(merchant, start_timestamp, end_timestamp, start, limit)`

## Lifecycle hooks

- Period close in recurring charge flow writes snapshot + statement (`PeriodClosed`).
- Cancellation writes current-period snapshot + statement (`Cancellation`).
- Subscriber refund withdrawal upserts statement with refund and marks `FinalSettlement`.

## Event coordination

Each statement upsert emits:

- Topic: `bill_stmt`
- Payload: `BillingStatementPersistedEvent { subscription_id, period_index, merchant, finalized_by }`

Off-chain systems can cross-check statements against existing events such as:

- `charged`
- `fee`
- `lifetime_cap_reached`
- `deposited`
- `migration_export` and snapshot exports

## Example query usage for Past Periods pages

1. Merchant Past Periods list:
   - Call `get_bill_stmts_by_merch_rng(merchant, from_ts, to_ts, start, limit)`
2. Subscription drill-down:
   - Call `get_bill_stmts_by_sub(subscription_id, start, limit)`
3. Single period detail:
   - Call `get_billing_statement(subscription_id, period_index)`
