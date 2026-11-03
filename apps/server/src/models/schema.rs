//! Diesel's view of `schema/*.sql`, written by hand to match it; the `migrate`
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
        archived_at -> Nullable<BigInt>,
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
        expires_at -> Nullable<BigInt>,
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
    q (id) {
        id -> Text,
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
    rpc_endpoints (chain, network) {
        chain -> Text,
        network -> Text,
        nodes -> Text,
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
    }
}

diesel::table! {
    trigger_run (id) {
        id -> Text,
        trigger -> Text,
        started_at -> BigInt,
        jobs -> Text,
    }
}

diesel::joinable!(transactions -> block (block));
diesel::joinable!(trigger_run -> triggers (trigger));
diesel::joinable!(leg -> transactions (transaction));
diesel::joinable!(leg -> block (block));
diesel::joinable!(wallet_index -> block (indexed_at));
diesel::joinable!(wallet_index -> wallet (wallet_id));

diesel::allow_tables_to_appear_in_same_query!(
    block,
    transactions,
    leg,
    wallet,
    wallet_index,
    triggers,
    trigger_run
);
