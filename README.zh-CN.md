# bedrock-leveldb

[English](README.md) | [简体中文](README.zh-CN.md)

`bedrock-leveldb` 是一个读优先的纯 Rust 原始 key/value 库，用于 Minecraft
Bedrock 世界数据库。它只处理存储层；区块、实体、玩家、NBT 等语义不在
本库范围内，应由应用层或领域层处理。

本 crate 可以读取 Bedrock/native LevelDB 的 manifest、WAL 和 table 文件。
写入 API 的定位更保守：由本 crate 写入或 flush 的数据使用本 crate 自己的
`BWLDB...` table/manifest 格式，不是可与其他 LevelDB 引擎互通的 native
LevelDB 输出。

维护者和贡献者请同时阅读[开发指南](docs/DEVELOPMENT.zh-CN.md)。

## 快速开始

```rust
use bedrock_leveldb::{
    Db, OpenOptions, ReadOptions, ScanMode, ScanPipelineOptions, VisitorControl, WriteOptions,
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
            pipeline: ScanPipelineOptions {
                queue_depth: 64,
                ..ScanPipelineOptions::default()
            },
            ..ReadOptions::default()
        },
        |key, value| {
            println!("{} -> {} bytes", String::from_utf8_lossy(key), value.len());
            Ok(VisitorControl::Continue)
        },
    )?;

    println!(
        "visited {} entries across {} tables on {} workers",
        outcome.visited, outcome.tables_scanned, outcome.worker_threads
    );

    db.put(b"tool_key".as_slice(), b"tool_value".as_slice(), WriteOptions::default())?;
    Ok(())
}
```

分析真实 Bedrock 世界时，建议设置 `OpenOptions::read_only = true` 且
`create_if_missing = false`。只读句柄不会初始化、repair、flush 或写入数据库目录。

## 支持范围

| 范围 | 状态 |
| --- | --- |
| native LevelDB manifest replay | 已实现查找 table 所需的元数据 |
| native LevelDB WAL replay | 已实现 WriteBatch replay |
| native LevelDB table 读取 | 支持 footer、index block、data block、restart array、internal key trailer |
| 压缩读取 | feature 开启时支持 Snappy、zlib、Bedrock raw deflate |
| Lazy point lookup | 支持 manifest range 过滤和 seeked block 读取 |
| Visitor scan | 支持 key、entry、prefix、顺序和 table-parallel 模式 |
| native block cache | 有界 decoded block cache |
| Bedrock chunk key helper | 解析和编码文档化的 LevelDB chunk key |
| 旧版 `LegacyTerrain` value | 校验并暴露早期 LevelDB 的 83,200 字节 terrain 布局 |
| 旧版 subchunk value | 识别 paletted subchunk，并暴露 pre-paletted block ID/metadata 数组 |
| 本 crate 写入 | 仅写入自定义 `BWLDB...` 格式 |
| 生产级 LevelDB compaction | 未实现 |
| 任意损坏数据库 repair | 部分实现，输出为自定义修复格式 |
| Pre-LevelDB world | 不支持；`chunks.dat` 和 `entities.dat` 不属于本 crate |
| `mmap` 读取路径 | feature 预留；默认是 seeked file I/O |

## API 说明

- `Db::open(path, OpenOptions)` 加载 `CURRENT`、manifest 元数据和 WAL overlay，
  不会急切物化所有 native table value。
- `Db::get(key)` 使用默认读取选项；`Db::get_with(key, ReadOptions)` 可覆盖
  checksum 和 cache 策略。
- 默认 `async` feature 下，`Arc<Db>` 提供 owned async 读取接口：
  `get_async`、`get_with_async`、`collect_keys_owned_async`、
  `collect_prefix_keys_owned_async` 和 `collect_prefix_owned_async`。这些接口内部使用
  Tokio `spawn_blocking`，适合 GUI 或服务端 runtime 避免阻塞前台任务。
- `Db::collect_keys_owned`、`Db::collect_prefix_keys_owned` 和
  `Db::collect_prefix_owned` 为常见索引路径直接返回 owned 数据，调用方不必手写
  visitor glue。
- `ReadOptions::pipeline` 控制本地 Rayon scan 调度。`queue_depth`、
  `table_batch_size` 和 `progress_interval` 为 0 时自动选择。`ScanOutcome`
  会报告 `tables_scanned`、`worker_threads`、`queue_wait_ms` 和 `cancel_checks`，
  便于按统计调优，而不是依赖跨机器固定耗时阈值。
- `Db::for_each_key`、`Db::for_each_entry`、`Db::for_each_prefix` 以 visitor
  方式流式返回 borrowed key 和 `Bytes` value。
- `Db::for_each_prefix_key` 是渲染索引推荐路径。只需要 key 时不再回调 value，
  native table 扫描也会直接 seek 到目标 prefix 范围。
- visitor 返回 `VisitorControl::Continue` 或 `VisitorControl::Stop`；正常提前
  停止体现在 `ScanOutcome` 中，不作为错误返回。
- `stats_fast()` 只读取元数据和 overlay；`stats_full()`、snapshot、物化
  iterator、repair、自定义 compact 都是显式昂贵路径。

### 迁移：全量 prefix value 扫描到 key-only scan

旧版渲染索引常常为了判断 chunk 是否有可渲染记录而读取 value：

```rust
let mut keys = Vec::new();
db.for_each_prefix(b"chunk-prefix", ReadOptions::default(), |key, _value| {
    keys.push(bytes::Bytes::copy_from_slice(key));
    Ok(bedrock_leveldb::VisitorControl::Continue)
})?;
```

现在应优先使用 key-only API：

```rust
let mut keys = Vec::new();
db.for_each_prefix_key(b"chunk-prefix", ReadOptions::default(), |key| {
    keys.push(bytes::Bytes::copy_from_slice(key));
    Ok(bedrock_leveldb::VisitorControl::Continue)
})?;
```

异步调用方应复用同一个数据库句柄，而不是每个请求重新 open：

```rust
let db = std::sync::Arc::new(Db::open("path/to/world/db", OpenOptions::default())?);
let keys = db
    .clone()
    .collect_prefix_keys_owned_async(
        bytes::Bytes::from_static(b"chunk-prefix"),
        ReadOptions::default(),
    )
    .await?;
```

## Bedrock 记录辅助解析

数据库 API 仍然保持 raw key/value。对于旧版 Bedrock LevelDB 世界，本 crate
额外提供存储层级的辅助类型，用来处理文档化的旧记录布局：

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

这些 helper 覆盖 Bedrock 存档格式历史中属于 LevelDB 时代的旧布局，包括
`LegacyTerrain` 和旧版 `SubChunkPrefix` payload family。它们不会解析
pre-LevelDB 的 `chunks.dat` / `entities.dat` 世界，也不会解析 NBT、actor
记录或游戏语义层面的区块内容。

## 日志

本项目是库 crate，只通过标准 `log` facade 发出诊断事件。库不会初始化全局
logger，也不会调用 `println!` 或 `eprintln!`。应用层可以自行接入
`env_logger`、`log4rs`、`tracing-log` 或自己的 logger：

```rust
fn main() -> bedrock_leveldb::Result<()> {
    // 仅作为示例：logger 应在应用入口配置。
    env_logger::init();

    let db = bedrock_leveldb::Db::open("path/to/world/db", Default::default())?;
    let _ = db.get(b"player_1")?;
    Ok(())
}
```

日志事件保持低噪声，不记录 raw value。当前主要覆盖数据库打开、manifest/WAL
replay、table scan、自定义 flush、repair 丢弃不可读文件、并行 worker、取消和
key-only prefix scan。使用 `tracing` 的应用可以通过 `tracing_log::LogTracer`
接入这些日志。

## 错误处理

所有可能失败的 API 返回 `bedrock_leveldb::Result<T>`，也就是
`Result<T, LevelDbError>`。`LevelDbError` 是结构化错误；应用层建议匹配
`ErrorKind` 并使用 `path()`，不要解析 display 字符串：

```rust
use bedrock_leveldb::{Db, ErrorKind, OpenOptions};

let result = Db::open(
    "missing-db",
    OpenOptions {
        read_only: true,
        create_if_missing: false,
        ..OpenOptions::default()
    },
);

let Err(error) = result else {
    panic!("missing database should fail");
};
assert_eq!(error.kind(), ErrorKind::NotFound);
assert!(error.path().is_some());
```

协作式 scan 取消返回 `ErrorKind::Cancelled`。只读句柄在 write、flush、repair
和自定义 compact 时返回 `ErrorKind::ReadOnly`。

## Features

| Feature | 默认 | 含义 |
| --- | --- | --- |
| `zlib` | 是 | 启用 zlib、Bedrock raw-deflate 解压，以及 zlib 自定义写入 |
| `snappy` | 是 | 启用 Snappy table 解压，以及 Snappy 自定义写入 |
| `async` | 是 | 通过 Tokio `spawn_blocking` 提供 `Db::open_async` |
| `mmap` | 否 | 为未来 mapped read path 预留 |
| `repair-tools` | 否 | 为更完整 repair 工具预留 |
| `bench` | 否 | 为 benchmark-only 代码路径预留 |

最低 Rust 版本为 1.87。

## 测试和 Benchmark

首次公开提交前使用以下检查：

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

Criterion suite 是合成 benchmark，会分开测 overlay hot read、flushed custom
table read、native table point/prefix read、WAL recovery，以及顺序扫描和
table-parallel 扫描。大型世界的真实性能仍建议在上层 crate 使用真实 Bedrock
fixture 验证，因为本 crate 不解释世界 key 或 NBT payload。

最近一次本地 benchmark：Windows，2026-05-01，rustc 1.93.1，Criterion
sample size 10，measurement time 2 秒。由于未安装 `gnuplot`，Criterion 使用
Plotters backend。native table benchmark 使用合成 native fixture，并关闭 decoded
block cache。本次运行未安装 logger backend，也就是库的默认使用方式。

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

本项目使用以下任一协议授权：

- Apache License, Version 2.0
- MIT license
