# Changelog

All notable changes to `bedrock-leveldb` are tracked here.

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
