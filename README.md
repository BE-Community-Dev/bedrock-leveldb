# bedrock-leveldb

[English](README.md) | [简体中文](README.zh-CN.md)

`bedrock-leveldb` is a read-first, pure Rust raw key/value library for
Minecraft Bedrock world databases. It focuses on the storage layer only:
chunk, actor, player, and NBT semantics are intentionally out of scope and
belong in application code or domain-specific layers.

The crate can read native Bedrock/LevelDB manifests, WAL records, and table
files. Its write APIs are intentionally limited for local tooling: data written
or flushed by this crate uses the crate's own `BWLDB...` table and manifest
format, not native LevelDB output for interchange with other engines.

## Quick Start

```rust
use bedrock_leveldb::{
    Db, OpenOptions, ReadOptions, ScanMode, VisitorControl, WriteOptions,
};

fn main() -> bedrock_leveldb::Result<()> {
    let db = Db::open("path/to/world/db", OpenOptions::default())?;

    if let Some(value) = db.get(b"player_1")? {
        println!("player_1 has {} raw bytes", value.len());
    }

    let outcome = db.for_each_prefix(
        b"player_",
        ReadOptions {
            scan_mode: ScanMode::ParallelTables,
            ..ReadOptions::default()
        },
        |key, value| {
            println!("{} -> {} bytes", String::from_utf8_lossy(key), value.len());
            Ok(VisitorControl::Continue)
        },
    )?;

    println!("visited {} entries", outcome.visited);

    db.put(b"tool_key".as_slice(), b"tool_value".as_slice(), WriteOptions::default())?;
    Ok(())
}
```

For read-only analysis of real Bedrock worlds, set `OpenOptions::read_only =
true` and `create_if_missing = false`. Read-only handles never initialize,
repair, flush, or write to the database directory.

## Supported Surface

| Area | Status |
| --- | --- |
| Native LevelDB manifest replay | Implemented for the metadata needed to find tables |
| Native LevelDB WAL replay | Implemented for write batches |
| Native LevelDB table reads | Footer, index block, data blocks, restart arrays, internal key trailer |
| Compression reads | Snappy, zlib, and Bedrock raw deflate when features are enabled |
| Lazy point lookup | Implemented with manifest range filtering and seeked block reads |
| Visitor scans | Key, entry, prefix, sequential, and table-parallel modes |
| Native block cache | Bounded decoded block cache |
| Bedrock chunk key helpers | Parse and encode documented LevelDB chunk keys |
| Legacy `LegacyTerrain` values | Validate and expose the 83,200-byte early LevelDB terrain layout |
| Legacy subchunk values | Classify paletted subchunks and expose pre-paletted block ID/metadata arrays |
| Writes by this crate | Custom `BWLDB...` format only |
| Production LevelDB compaction | Not implemented |
| Arbitrary corrupt database repair | Partial, writes custom repaired output |
| Pre-LevelDB worlds | Not supported; `chunks.dat` and `entities.dat` are outside this crate |
| `mmap` read path | Feature reserved; default path uses seeked file I/O |

## API Notes

- `Db::open(path, OpenOptions)` loads `CURRENT`, manifest metadata, and the WAL
  overlay. It does not eagerly materialize every native table value.
- `Db::get(key)` reads with default options. `Db::get_with(key, ReadOptions)`
  allows per-call checksum and cache policy.
- `Db::for_each_key`, `Db::for_each_entry`, and `Db::for_each_prefix` stream
  borrowed keys and `Bytes` values to visitors.
- Visitors return `VisitorControl::Continue` or `VisitorControl::Stop`; normal
  early termination is reported in `ScanOutcome`, not as an error.
- `stats_fast()` is metadata/overlay-only. `stats_full()`, snapshots,
  materialized iterators, repair, and custom compaction are explicit expensive
  paths.

## Bedrock Record Helpers

The database APIs stay raw key/value APIs. For old Bedrock LevelDB worlds, the
crate also provides storage-level helpers for documented record families:

```rust
use bedrock_leveldb::{
    BedrockKey, ChunkRecordTag, Db, LegacyTerrain, OpenOptions,
};

# fn example() -> bedrock_leveldb::Result<()> {
let db = Db::open("path/to/world/db", OpenOptions::default())?;

db.for_each_entry(Default::default(), |key, value| {
    if let BedrockKey::Chunk(chunk_key) = BedrockKey::parse(key) {
        if chunk_key.tag == ChunkRecordTag::LegacyTerrain {
            let terrain = LegacyTerrain::parse(value)?;
            let _block_id = terrain.block_id(0, 64, 0);
        }
    }
    Ok(bedrock_leveldb::VisitorControl::Continue)
})?;
# Ok(())
# }
```

The helpers cover the LevelDB-era legacy layouts described by the Bedrock
format history, including `LegacyTerrain` and old `SubChunkPrefix` payload
families. They intentionally do not parse pre-LevelDB `chunks.dat` /
`entities.dat` worlds, NBT payloads, actor records, or gameplay-level chunk
semantics.

## Logging

This is a library crate, so it only emits diagnostics through the standard
`log` facade. It does not initialize a global logger and never calls
`println!` or `eprintln!`. Applications can connect any compatible backend:

```rust
fn main() -> bedrock_leveldb::Result<()> {
    // Example only: choose env_logger, log4rs, tracing-log, or your own logger
    // at the application boundary.
    env_logger::init();

    let db = bedrock_leveldb::Db::open("path/to/world/db", Default::default())?;
    let _ = db.get(b"player_1")?;
    Ok(())
}
```

Log events are intentionally low-noise and avoid raw values. Useful events are
emitted around database open, manifest/WAL replay, table scans, custom flushes,
and repair paths that discard unreadable files.

## Errors

All fallible APIs return `bedrock_leveldb::Result<T>`, an alias for
`Result<T, LevelDbError>`. `LevelDbError` is structured; prefer matching
`ErrorKind` and using `path()` instead of parsing display strings:

```rust
use bedrock_leveldb::{Db, ErrorKind, OpenOptions};

let err = Db::open(
    "missing-db",
    OpenOptions {
        read_only: true,
        create_if_missing: false,
        ..OpenOptions::default()
    },
)
.expect_err("missing database should fail");

assert_eq!(err.kind(), ErrorKind::NotFound);
assert!(err.path().is_some());
```

Cooperative scan cancellation returns `ErrorKind::Cancelled`. Read-only handles
return `ErrorKind::ReadOnly` for writes, flushes, repair, and custom compaction.

## Features

| Feature | Default | Meaning |
| --- | --- | --- |
| `zlib` | yes | Enables zlib and Bedrock raw-deflate decompression plus zlib custom writes |
| `snappy` | yes | Enables Snappy table decompression plus Snappy custom writes |
| `async` | yes | Adds `Db::open_async` through Tokio `spawn_blocking` |
| `mmap` | no | Reserved for a future mapped read path |
| `repair-tools` | no | Reserved for expanded repair tooling |
| `bench` | no | Reserved for benchmark-only code paths |

MSRV is Rust 1.87.

## Testing And Benchmarks

Release checks used before the first public commit:

```text
cargo fmt --check
cargo clippy --all-features --all-targets -- -D warnings
cargo rustdoc --all-features -- -D missing_docs
cargo test --all-features
cargo test --no-default-features
cargo test --no-default-features --features zlib
cargo test --no-default-features --features snappy
cargo test --no-default-features --features async
cargo test --no-default-features --features mmap
cargo doc --all-features --no-deps
cargo package --allow-dirty
cargo bench --all-features
```

The Criterion suite is synthetic. It separates overlay hot reads, flushed
custom table reads, native table point/prefix reads, WAL recovery, and
sequential versus table-parallel scans. Large-world behavior should still be
validated with real Bedrock fixtures in higher-level crates because this crate
does not interpret world keys or NBT payloads.

Latest local benchmark run: Windows, May 1 2026, rustc 1.93.1,
Criterion sample size 10, 2 second measurement time. `gnuplot` was not
installed, so Criterion used the Plotters backend. The native table benchmark
uses synthetic native fixtures with the decoded block cache disabled. No logger
backend was installed for this run, which is the default library usage.

```text
bedrock_leveldb/write/batch_1000_overlay        [2.4738 ms 2.5905 ms 2.6781 ms]
bedrock_leveldb/get_point/overlay_hot           [85.575 ns 86.229 ns 87.213 ns]
bedrock_leveldb/get_point/custom_table          [4.5060 ms 4.6603 ms 4.9609 ms]
bedrock_leveldb/get_point/native_table          [4.8687 ms 5.0016 ms 5.3457 ms]
bedrock_leveldb/scan/custom_for_each_key        [4.3913 ms 4.4688 ms 4.6315 ms]
bedrock_leveldb/scan/custom_for_each_entry      [4.5432 ms 4.6145 ms 4.7531 ms]
bedrock_leveldb/scan/native_for_each_prefix     [6.2553 ms 6.4846 ms 6.6705 ms]
bedrock_leveldb/scan/native_parallel_tables     [3.2028 ms 3.2548 ms 3.3292 ms]
bedrock_leveldb/recover/wal_1000_overlay        [1.8688 ms 1.9349 ms 2.0575 ms]
```

## License

Licensed under either of:

- Apache License, Version 2.0
- MIT license
