# Logging

Injected structured events using the standard `log::Log` trait. No global logger,
runtime, database or SDK dependency; `Memory` keeps bounded process-local logs.

## Usage

```rust
use std::sync::Arc;
use logging::{Filter, Level, Logger, Memory};

let memory = Arc::new(Memory::default());
let logger = Logger::new(memory.clone())
    .with("chain", "bitcoin").with("network", "mainnet")
    .with("component", "rpc").with("node_id", "primary");
logger.event(Level::Warn, "rpc.failed", "RPC request failed", &[("kind", "timeout")]);
let page = memory.page(0, 100, &Filter::default());
assert_eq!(page.entries.len(), 1);
```

Supply any `Arc<dyn logging::Log>` for a custom sink. `Logger::default()` is a no-op.
Callbacks must return promptly and must not panic; queue external I/O in the sink.
Never emit credentials, endpoint URLs, payloads or provider error prose. Events
are notifications, not transactional delivery. The memory sink bounds record
text to 8 KiB, fields to 32, pages to 1,000, and reports truncation/retention gaps.
