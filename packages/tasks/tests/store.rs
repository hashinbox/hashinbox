//! The store's rules ([`tasks::store_contract`]) on `MemoryStore`.

tasks::store_contract!(async || ((), tasks::MemoryStore::new()));
