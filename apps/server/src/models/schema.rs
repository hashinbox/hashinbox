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
    }
}

diesel::table! {
    wallet (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        address -> Text,
        curve -> Text,
        secret -> Binary,
    }
}

diesel::table! {
    wallet_index (id) {
        id -> Text,
        chain -> Text,
        network -> Text,
        address -> Text,
        asset -> Text,
        birthday -> BigInt,
        indexed_at -> Nullable<Text>,
    }
}

diesel::table! {
    q (id) {
        id -> Text,
        lane -> Text,
        kind -> Text,
        state -> Integer,
        payload -> Text,
        run_at -> BigInt,
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

diesel::joinable!(transactions -> block (block));
diesel::joinable!(leg -> transactions (transaction));
diesel::joinable!(wallet_index -> block (indexed_at));

diesel::allow_tables_to_appear_in_same_query!(block, transactions, leg, wallet_index);
