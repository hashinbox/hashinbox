//! The schema files the server's one SQLite file is made of, embedded from
//! the repository's `schema/` directory: the one place that lists them.

/// Every schema file, in the order they are executed (`Db::migrate`).
pub(crate) const ALL: [&str; 3] = [
    include_str!("../../../schema/sdk.sql"),
    include_str!("../../../schema/server.sql"),
    include_str!("../../../schema/queue.sql"),
];
