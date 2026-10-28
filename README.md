# Irmin

A Rust library that downloads Chinese anime games. Manifest-based chunk downloads with zstd compression.

## What it does

- Downloads a fresh game build from a manifest of zstd-compressed chunks
- Updates an existing install by diffing the installed tag against the remote tag
- Pre-downloads upcoming versions as a patch package you apply on release
- Verifies file integrity, installs plugins and channel SDKs, applies HDiff patches
- Reports progress through a callback you provide

## Requirements

- Rust >= 1.92. We recommend the use of [rustup](https://rustup.rs) instead of installing rust directly

Irmin targets Linux.

## Use Irmin

First, add the crates:

```toml
irmin = { version = "1.0.0", package = "irmin_rs" }
```

next, create the Sophon download handler:

```rust
use irmin::{Sophon, DownloadHandle, SophonProgress, VerifyMode};

let handle = DownloadHandle::new();
let sophon = Sophon::builder("hk4e", "/path/to/game")
    .vo_lang("en-us")
    .verify_mode(VerifyMode::Full)
    .build();

sophon
    .download(&handle, |p: SophonProgress| {
        println!("{p:?}");
    })
    .await?;
```

`Sophon` owns the manifest-fetch client, the game identity, the install path, the resume-state directory, and the verify mode. Build it once and call any of `download`, `update`, `preinstall`, `apply_preinstall`, `check_update`, or `verify_integrity`. The builder defaults the client to a tuned HTTP/1.1 connection pool, `vo_lang` to `en-US`, `state_dir` to the install dir, and `verify_mode` to `Full`. Override any of them with `.client(...)`, `.vo_lang(...)`, `.state_dir(...)`, or `.verify_mode(...)`. Pass a `DownloadHandle` to any download operation. Clone the handle before the call and you can `handle.pause()`, `handle.resume()`, or `handle.cancel()` the in-flight operation from another task. No global registry tracks active downloads.

Each download, update, and preinstall writes a per-game state file into `state_dir`. irmin reuses the on-disk chunks when the remote manifest hash matches, and starts a fresh download when the hash or any identity field differs. irmin removes the state file on success. To inspect that state without a `Sophon`, call `irmin::load_download_state` or `irmin::state_file_path`.

For lower-level control, the `game_installer` module exposes `build_installers`, `install`, `preinstall_download`, `apply_preinstall`, `check_update`, and `verify_integrity`.

### Verification modes

The `VerifyMode` parameter sets how much integrity checking irmin performs during download:

| Mode | Behavior | Crash-resume | CPU cost |
|------|----------|-------------|----------|
| `Full` | Hash every chunk during download and assembly. Full crash-resume via bitmap. | Yes | ~6.8 s/GiB |
| `Deferred` | Skip per-chunk hashing. Verify only the final assembled file. All-eager pipeline (no intermediate .zstd files). No crash-resume. | No | ~3.4 s/GiB |
| `None` | Size-only checks. No hashing. No crash-resume. | No | ~3.0 s/GiB |

Interrupt a `Deferred` or `None` download and irmin re-downloads the incomplete files from scratch on the next run. `Full` mode resumes from where it left off.

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

`SophonProgress` is a serde-serializable enum. Each variant reports the relevant counters; `Finished` signals a clean end. Errors come back as `SophonError` from the pipeline.

### Manual integrity repair

`verify_integrity` hashes installed files rather than trusting persisted size/mtime cache hits, regardless of the builder's download verification mode. It repairs missing or damaged files, respects the selected voice language and the game's removed-resource filters, and regenerates launcher-owned metadata. Repair errors and worker failures return `Err`; `Finished` is emitted only after repair and metadata generation succeed. Plugins and SDKs are outside this operation's scope.

### Applying preinstalls after release

Completed preinstalls now save the exact target manifests and each manifest's chunk-download information in their package state. Applying these packages does not need the pre-download branch, build metadata, or manifest endpoints to remain available, and cannot silently switch to a newer pre-download target.

This is **metadata independence, not fully offline installation**. New files and patch-repair fallbacks can still need payload chunks from the CDN. An interrupted, incomplete preinstall still needs upstream metadata to rebuild its download plan after restart.

Older package states remain readable. Irmin tries to recover their exact saved target and manifest IDs before changing game files; if that metadata is gone, apply returns an actionable error and preserves the downloaded package. Legacy packages retain their recorded voice scope: omitted voice packs generate a warning and need a subsequent normal update/repair. Legacy deletion lists lack scope information, so obsolete files are conservatively retained rather than risking deletion of an omitted voice pack; they may require separate cleanup.

Successful apply removes that package's metadata and marker. Shared patch/chunk payloads are retained because other completed or interrupted packages may still need them. Failed or cancelled applies retain metadata for retry.

## How the pipeline works

A download starts by fetching a manifest from the game's delivery API. The manifest lists every file, its size, and the chunks that compose it. Irmin checks what is already on disk, decides which chunks it still needs, and pulls them in parallel. In `Full` mode, chunks arrive zstd-compressed and irmin writes them to temp files. The assembler stitches them into final files, runs integrity checks, and installs any plugins or SDKs the manifest requests. In `Deferred`/`None` modes, irmin decompresses chunks inline into their final positions (the "eager" path), skipping intermediate IO.

Updates skip unchanged files by comparing the installed version tag against the remote tag. Preinstall downloads the next version's patch before release. When the server switches to that version, apply the patch and the game runs.

## Benchmarks

```sh
cargo bench --features benchmark
```

`benches/sophon_bench.rs` measures the hot paths: manifest parsing, chunk scheduling, assembly, and HDiff patching. `src/bin/bench_memory.rs` samples peak RSS during installs. Enable the `sophon-profiling` cargo feature for jemalloc allocator stats.

## License

BSD-3-Clause. The Elysiae Project.
