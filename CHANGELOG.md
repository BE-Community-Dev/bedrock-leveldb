# Changelog

All notable changes to `bedrock-leveldb` are tracked here.

## 0.2.0 - 2026-05-07

### Added

- Added native LevelDB write APIs: `Db::write_batch_native`,
  `Db::flush_memtable`, `Db::compact_range_native`, and `Db::recover_native`.
- Added standard LevelDB WAL batch append, native `.ldb` flush, manifest
  version edit persistence, sequence-number visibility, and deletion tombstone
  replay for the v0.2 write path.
- Added key-only prefix scans with `Db::for_each_prefix_key` so render indexes
  can discover chunk records without materializing unrelated values.
- Added owned async read helpers for shared handles:
  `Arc<Db>::get_async`, `Arc<Db>::get_with_async`,
  `Arc<Db>::collect_keys_owned_async`,
  `Arc<Db>::collect_prefix_keys_owned_async`, and
  `Arc<Db>::collect_prefix_owned_async`.
- Added owned sync collectors: `collect_keys_owned`,
  `collect_prefix_keys_owned`, and `collect_prefix_owned`.
- Added `ReadOptions::pipeline` / `ScanPipelineOptions` for bounded queue depth,
  table batch sizing, and progress cadence in parallel scans.
- Added `ScanOutcome` diagnostics for `tables_scanned`, `worker_threads`,
  `queue_wait_ms`, and `cancel_checks`.
- Added `get_many_owned` regression coverage for early Bedrock
  `LegacyTerrain` (`0x30`) keys, preserving missing/duplicate/input ordering.
- Reaffirmed the storage-layer contract for renderer coordinate debugging:
  `get_many_owned` returns raw `LegacyTerrain`, legacy `SubChunkPrefix`, and
  modern `SubChunkPrefix` bytes unchanged; coordinate interpretation belongs to
  `bedrock-world` and `bedrock-render` tests.
- Clarified that legacy biome priority is also a world/render semantic; this
  crate only preserves the raw `LegacyTerrain` bytes and input ordering.
- Documented the old-world LevelDB boundary: native zlib tag `2`, raw deflate
  tag `4`, WAL + `.ldb`, and exact `LegacyTerrain` reads are supported here;
  pre-LevelDB `chunks.dat` files remain a `bedrock-world` backend concern.
- Corrected the `LegacyTerrain` helper's biome accessor so the final 1024-byte
  tail is exposed as `[biome_id, red, green, blue]` samples, with
  `biome_color_at` returning compatibility `0x00RRGGBB`.
- Added clearer Rayon worker logging around scan start/finish, prefix scans,
  progress, queue backpressure, and cancellation-sensitive paths through the
  `log` facade.

### Breaking Changes

- Visitor callbacks used with table-parallel APIs must be `Send` because scans
  now run on a local Rayon thread pool.
- Struct literals for `ReadOptions` must set `pipeline` or use
  `..ReadOptions::default()`.
- New writes now use native LevelDB-compatible files. The old `BWLDB...` format
  remains readable for migration/backward compatibility, but is no longer the
  default flush output.

### Migration Notes

- Render and world callers that previously used `for_each_prefix` only to collect
  keys should migrate to `for_each_prefix_key`.
- Async callers should wrap `Db` in `Arc` and use the owned async helpers instead
  of reopening the database per request.
- Tune `ScanPipelineOptions` only after looking at `ScanOutcome.queue_wait_ms`
  and `worker_threads`; the default zero values are automatic and usually best
  for interactive render indexing.

## 0.1.0 - 2026-05-01

### Added

- Initial public crate-ready implementation of a pure Rust LevelDB-style backend
  for Minecraft Bedrock world databases.
- Read-first native LevelDB support for manifest, WAL, table blocks, prefix
  scans, cache controls, cooperative scan cancellation, and progress reporting.
- Custom write, delete, batch, flush, and reopen support using this crate's
  documented `BWLDB...` table format.
- Bedrock LevelDB key helpers plus documented legacy `LegacyTerrain` and
  pre-paletted `SubChunkPrefix` payload helpers.
- `log` facade diagnostics, structured errors, CI, Criterion benchmarks, package
  metadata, and English/Simplified Chinese documentation.

### Notes

- Native LevelDB-compatible writes and compaction are intentionally not part of
  this release.
- Pre-LevelDB Bedrock files such as `chunks.dat` and `entities.dat` are outside
  this crate's storage scope.
