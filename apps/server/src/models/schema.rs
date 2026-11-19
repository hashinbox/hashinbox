//! Diesel's view of `init.sql`, written by hand to match it; the `migrate`
//! command executes the SQL files, nothing here creates a table.

diesel::table! {
    block (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        position -> BigInt,
        height -> BigInt,
        hash -> Binary,
        parent -> Nullable<Text>,
        parent_position -> Nullable<BigInt>,
        parent_hash -> Nullable<Binary>,
        timestamp -> Nullable<BigInt>,
        reorged_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    #[sql_name = "transaction"]
    transactions (id) {
        id -> Text,
        block -> Nullable<Text>,
        chain -> Text,
        network -> Text,
        txid -> Text,
        order -> Nullable<BigInt>,
        state -> Integer,
        reason -> Nullable<Text>,
        bytes -> Binary,
        sent_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    leg (id) {
        id -> Text,
        block -> Nullable<Text>,
        transaction -> Nullable<Text>,
        leg_index -> BigInt,
        kind -> Integer,
        address -> Text,
        asset -> Text,
        amount -> Text,
        coin_txid -> Nullable<Text>,
        coin_index -> Nullable<BigInt>,
        wallet_index_id -> Text,
        position -> Nullable<BigInt>,
        seq -> Nullable<BigInt>,
        total -> Nullable<Text>,
        total_received -> Nullable<Text>,
        total_sent -> Nullable<Text>,
        archived_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    wallet (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        address -> Text,
        curve -> Nullable<Text>,
        secret -> Nullable<Binary>,
        tags -> Text,
        external_id -> Nullable<Text>,
    }
}

diesel::table! {
    wallet_index (id) {
        id -> Text,
        wallet_id -> Text,
        asset -> Text,
        requested_from -> BigInt,
        birthday -> BigInt,
        indexed_at -> Nullable<Text>,
        state -> Integer,
        reason -> Nullable<Text>,
    }
}

diesel::table! {
    network (chain, name) {
        chain -> Text,
        #[sql_name = "network"]
        name -> Text,
        chain_id -> Nullable<Text>,
        testnet -> Bool,
        enabled -> Bool,
        block_time_ms -> BigInt,
        confirmations -> BigInt,
        max_depth -> Nullable<BigInt>,
        explorer_address -> Nullable<Text>,
        explorer_transaction -> Nullable<Text>,
        settings -> Text,
    }
}

diesel::table! {
    network_asset (chain, network, asset) {
        chain -> Text,
        network -> Text,
        asset -> Text,
        kind -> Text,
        contract -> Nullable<Text>,
        ticker -> Text,
        decimals -> BigInt,
        display_decimals -> Nullable<BigInt>,
    }
}

diesel::table! {
    network_index (chain, network) {
        chain -> Text,
        network -> Text,
        tip -> Nullable<BigInt>,
        confirmed -> Nullable<BigInt>,
        indexed -> Nullable<BigInt>,
        checked_at -> Nullable<BigInt>,
        blocks -> BigInt,
        reorgs -> BigInt,
        failures -> BigInt,
    }
}

diesel::table! {
    /// Read-only view `transaction_with_legs`: a `transaction` row, its
    /// block's position and its legs as a JSON array.
    transaction_with_legs (id) {
        id -> Text,
        block -> Nullable<Text>,
        chain -> Text,
        network -> Text,
        txid -> Text,
        order -> Nullable<BigInt>,
        state -> Integer,
        reason -> Nullable<Text>,
        bytes -> Binary,
        sent_at -> Nullable<BigInt>,
        position -> Nullable<BigInt>,
        legs -> Text,
    }
}

diesel::table! {
    /// Read-only view `wallet_index_with_balance`: a `wallet_index` row with
    /// its wallet's address, progress and balances. Its `chain`,
    /// `network`, `behind_tip` and `finality_at` are left out: diesel's
    /// 16-column limit (`IndexRow` reads them by name).
    wallet_index_with_balance (id) {
        id -> Text,
        wallet_id -> Text,
        asset -> Text,
        requested_from -> BigInt,
        birthday -> BigInt,
        indexed_at -> Nullable<Text>,
        state -> Integer,
        reason -> Nullable<Text>,
        address -> Text,
        keyed -> Bool,
        indexed_through -> Nullable<BigInt>,
        balance_confirmed -> Nullable<Text>,
        balance_confirmed_received -> Nullable<Text>,
        balance_confirmed_sent -> Nullable<Text>,
        balance_confirmed_spendable -> Nullable<Text>,
        balance_pending -> Nullable<Text>,
    }
}

diesel::table! {
    q (id) {
        id -> BigInt,
        topic -> Text,
        kind -> Text,
        status -> Integer,
        run_at -> BigInt,
        deadline -> Nullable<BigInt>,
        attempts -> Integer,
        reason -> Nullable<Text>,
        input -> Text,
        state -> Nullable<Text>,
        created_at -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    network_endpoint (id) {
        id -> BigInt,
        chain -> Text,
        network -> Text,
        url -> Text,
        position -> BigInt,
    }
}

diesel::table! {
    network_endpoint_route (endpoint_id, method) {
        endpoint_id -> BigInt,
        method -> Text,
    }
}

diesel::table! {
    #[sql_name = "trigger"]
    triggers (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        group_id -> Text,
        selector -> Text,
        filter -> Nullable<Text>,
        check -> Text,
        action -> Text,
        enabled -> Bool,
        created_at -> BigInt,
        updated_at -> BigInt,
        last_run -> Nullable<Text>,
        last_round -> Nullable<Text>,
    }
}

diesel::table! {
    transaction_forgotten (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        txid -> Text,
        reason -> Text,
        forgotten_at -> BigInt,
        bytes -> Binary,
        legs -> Text,
    }
}

diesel::table! {
    transaction_index (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        txid -> Text,
        broadcasted_at -> BigInt,
        expires_height -> Nullable<BigInt>,
        indexed_at -> Nullable<Text>,
        found -> Nullable<Text>,
        state -> Integer,
        reason -> Nullable<Text>,
    }
}

diesel::table! {
    /// Read-only view `transaction_index_with_blocks`: a `transaction_index`
    /// row, whether its found transaction is kept reverted, and its
    /// network's confirmed position.
    transaction_index_with_blocks (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        txid -> Text,
        broadcasted_at -> BigInt,
        expires_height -> Nullable<BigInt>,
        indexed_at -> Nullable<Text>,
        found -> Nullable<Text>,
        state -> Integer,
        reason -> Nullable<Text>,
        reverted -> Bool,
        confirmed -> Nullable<BigInt>,
    }
}

diesel::table! {
    trigger_run (id) {
        id -> Text,
        trigger -> Text,
        started_at -> BigInt,
        jobs -> Text,
        stats -> Text,
    }
}

diesel::joinable!(transactions -> block (block));
diesel::joinable!(trigger_run -> triggers (trigger));
diesel::joinable!(leg -> transactions (transaction));
diesel::joinable!(leg -> block (block));
diesel::joinable!(wallet_index -> block (indexed_at));
diesel::joinable!(wallet_index -> wallet (wallet_id));
diesel::joinable!(network_endpoint_route -> network_endpoint (endpoint_id));

diesel::allow_tables_to_appear_in_same_query!(
    block,
    transactions,
    transaction_index,
    transaction_index_with_blocks,
    leg,
    wallet,
    wallet_index,
    triggers,
    trigger_run,
    network_endpoint,
    network_endpoint_route
);
