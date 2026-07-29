# Irmin

A Rust library that downloads Chinese anime games. Manifest-based chunk downloads with zstd compression.

## What it does

- Downloads a fresh game build from a manifest of zstd-compressed chunks
- Updates an existing install by diffing the installed tag against the remote tag
- Pre-downloads upcoming versions as a patch package you apply on release
- Verifies file integrity, installs plugins and channel SDKs, applies HDiff patches
- Reports progress through a callback you provide

## Requirements

- [Rust](https://rustup.rs) 1.92 or newer

Irmin is intended for use on Linux.

## Use it from Rust

Add the crate and call the high-level convenience functions:

```rust
use std::sync::Arc;
use irmin::{sophon_download, SophonProgress};

let client = reqwest::Client::new();
let on_progress: irmin::ProgressUpdater = Arc::new(|p: SophonProgress| {
    println!("{p:?}");
});

sophon_download(&client, "hk4e", "en-us", "/path/to/game", on_progress).await?;
```

The convenience functions take a `&reqwest::Client` explicitly, so you control connection pooling. For lower-level control, the `game_installer` module exposes `build_installers`, `install`, `preinstall_download`, `apply_preinstall`, `check_update`, and `verify_integrity` directly.

Supported game identifiers:

- `hk4e`
- `hkrpg`
- `nap`
- `bh3`

Supported voice pack locales:

- `en-us`
- `zh-cn`
- `zh-tw`
- `ko-kr`
- `ja-jp`

`SophonProgress` is a serde-serializable enum. Each variant reports the relevant counters; `Finished` signals a clean end. Errors come back as `SophonError` from the underlying pipeline.

## How the pipeline works

A download starts by fetching a manifest from the game's delivery API. The manifest lists every file, its size, and the chunks that compose it. Irmin checks what is already on disk, decides which chunks it still needs, and pulls them in parallel. Chunks arrive zstd-compressed; Irmin writes them to temp files, and the assembler then stitches those chunks into final files, runs integrity checks, and installs any plugins or SDKs the manifest requests.

Updates skip unchanged files by comparing the installed version tag against the remote tag. Preinstall goes one step further and downloads the next version's patch ahead of time, so when the server flips over to that version you just apply the patch and you are running.

## Command-line binaries

For testing from the terminal, the crate ships with three command-line mirrors. Build them with `--features download-cli`:

```sh
cargo run --release --features download-cli --bin download_version -- hk4e en-us 6.7.0 /path/to/game
cargo run --release --features download-cli --bin download_update -- hk4e en-us 6.7.0 /path/to/game
cargo run --release --features download-cli --bin download_preinstall -- hk4e en-us /path/to/game
```

`download_version` takes a game ID, voice locale, tag, and game directory. `download_update` works the same way but the tag argument is your current installed version. `download_preinstall` skips the tag since it pulls the latest remote version directly.

All three write a state file next to the game directory. If a run is interrupted partway, the next invocation picks up where it left off.

## Benchmarks

```sh
cargo bench --features benchmark
```

`benches/sophon_bench.rs` measures the hot paths: manifest parsing, chunk scheduling, assembly, and HDiff patching. `src/bin/bench_memory.rs` samples peak RSS during install operations. For deeper memory inspection, enable the `sophon-profiling` cargo feature to turn on jemalloc allocator stats.

## License

BSD-3-Clause. The Elysiae Project.
