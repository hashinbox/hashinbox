//! Diesel's view of `init.sql`, written by hand to match it; `Db::open_or_create`
//! executes the SQL files on a new file, nothing here creates a table.

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
        broadcasted_at -> Nullable<BigInt>,
        position -> Nullable<BigInt>,
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
        amount_usd -> Nullable<Text>,
        total_received_usd -> Nullable<Text>,
        total_sent_usd -> Nullable<Text>,
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
        indexed_at -> Nullable<BigInt>,
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
        tip -> BigInt,
        confirmed -> BigInt,
        indexed -> Nullable<BigInt>,
        checked_at -> Nullable<BigInt>,
        blocks -> BigInt,
        reorgs -> BigInt,
        failures -> BigInt,
    }
}

diesel::table! {
    /// Read-only view `transaction_with_legs`: a `transaction` row and its
    /// legs as a JSON array.
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
        broadcasted_at -> Nullable<BigInt>,
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
        indexed_at -> Nullable<BigInt>,
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
    network_endpoint_stats (endpoint_id) {
        endpoint_id -> BigInt,
        calls -> BigInt,
        failed_calls -> BigInt,
        updated_at -> BigInt,
    }
}

diesel::table! {
    /// Read-only view `network_state`: a `network` row, and its assets,
    /// routes, index and nodes as JSON.
    network_state (chain, network) {
        chain -> Text,
        network -> Text,
        chain_id -> Nullable<Text>,
        testnet -> Bool,
        enabled -> Bool,
        block_time_ms -> BigInt,
        confirmations -> BigInt,
        max_depth -> Nullable<BigInt>,
        explorer_address -> Nullable<Text>,
        explorer_transaction -> Nullable<Text>,
        settings -> Text,
        assets -> Text,
        routes -> Text,
        index -> Nullable<Text>,
        endpoints -> Text,
    }
}

diesel::table! {
    network_endpoint_route (endpoint_id, method) {
        endpoint_id -> BigInt,
        method -> Text,
    }
}

diesel::table! {
    network_endpoint_failure (id) {
        id -> BigInt,
        endpoint_id -> BigInt,
        at -> BigInt,
        kind -> Text,
        message -> Text,
    }
}

diesel::table! {
    log (id) {
        id -> BigInt,
        at -> BigInt,
        level -> Text,
        target -> Text,
        message -> Nullable<Text>,
        fields -> Text,
        chain -> Nullable<Text>,
        network -> Nullable<Text>,
        job_id -> Nullable<BigInt>,
        wallet_id -> Nullable<Text>,
        transaction_id -> Nullable<Text>,
        trigger_id -> Nullable<Text>,
        market -> Nullable<Text>,
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
    transaction_index (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        txid -> Text,
        broadcasted_at -> BigInt,
        expires_height -> Nullable<BigInt>,
        indexed_at -> Nullable<BigInt>,
        found_at -> Nullable<BigInt>,
        state -> Integer,
        reason -> Nullable<Text>,
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

diesel::table! {
    market (name) {
        name -> Text,
        feed -> Bool,
        updated_at -> BigInt,
    }
}

diesel::table! {
    market_candle_1h (market, asset, start) {
        market -> Text,
        asset -> Text,
        start -> BigInt,
        open -> Text,
        high -> Text,
        low -> Text,
        close -> Text,
        updated_at -> BigInt,
    }
}

diesel::table! {
    #[sql_name = "user"]
    users (id) {
        id -> Text,
        email -> Text,
        name -> Text,
        password_hash -> Nullable<Text>,
        scopes -> Text,
        session_secret -> Binary,
        created_at -> BigInt,
        invalidated_at -> Nullable<BigInt>,
        disabled_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    user_key (id) {
        id -> Text,
        user_id -> Text,
        name -> Text,
        secret_hash -> Binary,
        secret -> Text,
        scopes -> Text,
        created_at -> BigInt,
        expires_at -> Nullable<BigInt>,
        invalidated_at -> Nullable<BigInt>,
        revoked_at -> Nullable<BigInt>,
        last_used_at -> Nullable<BigInt>,
    }
}

diesel::table! {
    user_2fa (user_id) {
        user_id -> Text,
        secret -> Binary,
        confirmed_at -> Nullable<BigInt>,
        last_step -> Nullable<BigInt>,
        created_at -> BigInt,
    }
}

diesel::table! {
    setting (key) {
        key -> Text,
        value -> Text,
        public -> Bool,
        updated_at -> BigInt,
    }
}

diesel::joinable!(transactions -> block (block));
diesel::joinable!(trigger_run -> triggers (trigger));
diesel::joinable!(leg -> transactions (transaction));
diesel::joinable!(leg -> block (block));
diesel::joinable!(wallet_index -> wallet (wallet_id));
diesel::joinable!(network_endpoint_route -> network_endpoint (endpoint_id));
diesel::joinable!(network_endpoint_failure -> network_endpoint (endpoint_id));

diesel::allow_tables_to_appear_in_same_query!(
    block,
    transactions,
    transaction_index,
    network_index,
    leg,
    wallet,
    wallet_index,
    triggers,
    trigger_run,
    network_endpoint,
    network_endpoint_route,
    network_endpoint_failure,
    network_endpoint_stats,
    log
);
