# Irmin

Irmin is a native Node.js module that downloads Chinese anime games. A Rust core handles manifests, chunk fetching, and patching, and a napi-rs boundary exposes that core to JavaScript with types intact.

## What it does

- Downloads a fresh game build from a manifest of zstd-compressed chunks
- Updates an existing install by diffing the installed tag against the remote tag
- Pre-downloads upcoming versions as a patch package you apply on release
- Verifies file integrity, installs plugins and channel SDKs, applies HDiff patches
- Reports progress as a JSON event stream you consume from JS

## Requirements

Before you start, make sure these are installed:

- [Rust](https://rustup.rs) 1.92 or newer
- [Node.js](https://nodejs.org) 20 or newer

Irmin intended for use on Linux.

## Setup

Corepack ships with Node but is turned off by default, so you will need to enable it first and let it pin a Yarn version for the project:

```sh
corepack enable
corepack prepare yarn@4 --activate
```

With that done, install the dependencies from the project root:

```sh
yarn install
```

## Build

```sh
yarn build
```

This compiles the Rust crate as a `cdylib` and emits two files in `dist/`:

- `irmin.node`: the native binary your app loads
- `index.d.ts`: TypeScript declarations for every exported function

If you are iterating and want a faster, unoptimized build, use the debug variant instead:

```sh
yarn build:debug
```

## Use it from JavaScript

Import the module from your Node.js app:

```typescript
import {
  initClient,
  sophonDownload,
  sophonUpdate,
  sophonPreinstall,
  sophonApplyPreinstall,
  sophonCheckUpdate,
  sophonPause,
  sophonResume,
  sophonCancel,
} from "irmin";
```

Call `initClient()` once at startup to initialize the HTTP client, and you are ready to start a download:

```typescript
await sophonDownload("hk4e", "en-us", "/path/to/game", (progressJson) => {
  const event = JSON.parse(progressJson);
  // event.type is one of: fetchingManifest | calculatingDownloads | downloading |
  //   paused | assembling | checkingFiles | verifying | warning | error |
  //   installingPlugins | installingSdks | downloadingPlugin |
  //   applyingPreinstall | finished
});
```

The arguments are, in order, a game identifier such as `hk4e`, a voice pack locale such as `en-us`, the game directory on disk, and a callback. Irmin hands the callback a JSON string for each progress event, which you parse into the typed event shown in the comment above.

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

For checking whether an update is available, `sophonCheckUpdate` returns an `UpdateInfoNapi` object describing the current and remote tags, any preinstall that is ready, and the compressed and decompressed byte sizes for both the update and the preinstall.

Once a download is underway, `sophonPause`, `sophonResume`, and `sophonCancel` let you control it without restarting the process.

## How the pipeline works

A download starts by fetching a manifest from the game's delivery API. The manifest lists every file, its size, and the chunks that compose it. Irmin checks what is already on disk, decides which chunks it still needs, and pulls them in parallel. Chunks arrive zstd-compressed; Irmin writes them to temp files, and the assembler then stitches those chunks into final files, runs integrity checks, and installs any plugins or SDKs the manifest requests.

Updates skip unchanged files by comparing the installed version tag against the remote tag. Preinstall goes one step further and downloads the next version's patch ahead of time, so when the server flips over to that version you just apply the patch and you are running.

## Command-line binaries

For testing from the terminal, the napi surface has three command-line mirrors. Build them with `--features download-cli`:

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
