//! The server's tests, compiled into the binary's test target (the server has
//! no library): the suite (`cases`, over mock chains through `support`),
//! and, until it replaces them, the stores and reads on SQLite (`storage`),
//! the send job's first run (`send`) and its checks (`check`) and the
//! routes (`routes`) over one scripted network (`support::scripted`); the
//! log message catalog against the event codes (`events`); the market job
//! (`market`). Offline.

mod cases;
mod check;
mod events;
mod market;
mod pipe;
mod routes;
mod send;
mod storage;
pub(crate) mod support;
