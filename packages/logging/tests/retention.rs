//! Injected sink, filtering and bounded retention contracts.
use logging::{Filter, Level, Log, Logger, Memory, Metadata, Record};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[test]
fn filters_combine_without_crossing_nodes_or_networks() {
    let memory = Arc::new(Memory::default());
    let root = Logger::new(memory.clone()).with("chain", "bitcoin");
    for network in ["mainnet", "testnet"] {
        for node in ["a", "b"] {
            root.with("network", network)
                .with("node_id", node)
                .with("component", "rpc")
                .event(Level::Warn, "rpc.failed", "Failed", &[]);
        }
        root.with("network", network)
            .with("component", "indexing")
            .event(Level::Info, "indexing.ready", "Ready", &[]);
    }
    let selected = Filter(BTreeMap::from([
        ("network".into(), "mainnet".into()),
        ("component".into(), "rpc".into()),
        ("node_id".into(), "b".into()),
    ]));
    assert_eq!(memory.page(0, 100, &selected).entries.len(), 1);
    memory.clear(&selected);
    let all = memory.page(0, 100, &Filter::default());
    assert_eq!(all.entries.len(), 5);
    assert!(all.gap);
}

#[test]
fn count_bytes_and_record_size_are_bounded_and_eviction_is_visible() {
    let memory = Arc::new(Memory::new(2, 32));
    let log = Logger::new(memory.clone());
    log.event(Level::Info, "a", "first", &[]);
    log.event(Level::Info, "a", "second", &[]);
    log.event(Level::Info, "a", "third", &[]);
    let page = memory.page(0, 20, &Filter::default());
    assert!(page.gap);
    assert_eq!(page.entries.len(), 2);
    log.event(Level::Info, "a", &"界".repeat(10_000), &[]);
    let page = memory.page(0, 20, &Filter::default());
    assert_eq!(page.entries.len(), 1);
    assert!(page.entries[0].truncated);
    assert!(page.entries[0].message.len() <= 31);
}

#[test]
fn concurrent_writers_preserve_strict_cursor_order() {
    let memory = Arc::new(Memory::default());
    let log = Logger::new(memory.clone());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let log = log.clone();
            scope.spawn(move || {
                for _ in 0..100 {
                    log.event(Level::Info, "x", "x", &[]);
                }
            });
        }
    });
    let page = memory.page(0, 1000, &Filter::default());
    assert_eq!(page.entries.len(), 400);
    assert!(
        page.entries
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert!(
        memory
            .page(page.next, 100, &Filter::default())
            .entries
            .is_empty()
    );
}

#[test]
fn custom_sink_is_explicit_and_needs_no_global_registration() {
    struct Sink(AtomicUsize);
    impl Log for Sink {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() <= Level::Warn
        }
        fn log(&self, _: &Record<'_>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn flush(&self) {}
    }
    let sink = Arc::new(Sink(AtomicUsize::new(0)));
    let logger = Logger::new(sink.clone());
    logger.event(Level::Info, "ignored", "Ignored", &[]);
    logger.event(Level::Warn, "failed", "Failed", &[]);
    Logger::default().event(Level::Error, "ignored", "Ignored", &[]);
    assert_eq!(sink.0.load(Ordering::Relaxed), 1);
}
