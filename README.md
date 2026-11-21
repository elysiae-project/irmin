# Irmin

Irmin is a Rust library for installing and updating supported anime games on Linux. It downloads manifest-based chunks, assembles game files, and reports progress to your application.

## Requirements

- Linux
- Rust 1.92 or later

## Add Irmin

Add the crate to `Cargo.toml`:

```toml
[dependencies]
irmin = { package = "irmin_rs", version = "1.1" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

## Download a game

Create one `Sophon` for each game install. The game code identifies the game, and the second argument is the directory Irmin will create or update.

```rust
use irmin::{DownloadHandle, Sophon, SophonProgress};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sophon = Sophon::builder("hk4e", "/games/example")
        .vo_lang("en-us")
        .build();
    let handle = DownloadHandle::new();

    sophon
        .download(&handle, |event: SophonProgress| {
            match event {
                SophonProgress::Downloading {
                    downloaded_bytes,
                    total_bytes,
                    speed_bps,
                    ..
                } => println!("{downloaded_bytes}/{total_bytes} bytes ({speed_bps:.0} B/s)"),
                SophonProgress::Finished => println!("Operation ended"),
                other => println!("{other:?}"),
            }
        })
        .await?;

    Ok(())
}
```

The callback runs on worker tasks, so keep it thread-safe and avoid blocking work. It receives `SophonProgress` events such as `FetchingManifest`, `Downloading`, `Assembling`, and `Warning`. Treat the awaited result as the success signal: the method returns `Ok(())` on success or a `SophonError` on failure. `Finished` marks the end of the operation's progress events.

## Choose an operation

| Call | Use it for |
| --- | --- |
| `download(&handle, callback)` | Install a fresh copy. |
| `update(&handle, callback)` | Update an existing install. Returns `NoInstalledVersion` if none exists. |
| `check_update()` | Check for update or preinstall availability and sizes. |
| `preinstall(&handle, callback)` | Download an available version's preinstall package. |
| `apply_preinstall(tag, &handle, callback)` | Apply a downloaded package after release. |
| `verify_integrity(callback)` | Check installed files and repair missing or damaged files. |

Use `update` for routine upgrades. Use `preinstall` and `apply_preinstall` only when your app offers a pre-download workflow. Applying a completed package can still download payload chunks from the CDN.

## Configure the installer

The builder requires a game identifier and install directory. These options are available:

| Option | Default | Purpose |
| --- | --- | --- |
| `.vo_lang(locale)` | `en-US` | Select the voice-over locale. |
| `.verify_mode(mode)` | `VerifyMode::Full` | Choose download integrity checks. |
| `.state_dir(path)` | Install directory | Store resume state separately, such as in app data. |
| `.client(client)` | Tuned HTTP client | Override the client used for manifest requests. |

Supported game codes: `hk4e`, `hkrpg`, `nap`, `bh3`. Supported voice locales: `en-us`, `zh-cn`, `zh-tw`, `ko-kr`, `ja-jp`.

`VerifyMode::Full` hashes chunks and supports crash resume. `Deferred` checks completed files and restarts incomplete files from scratch after interruption. `None` checks sizes without hashing and also restarts incomplete files from scratch. Keep `Full` unless your application has a specific reason to trade integrity checks or resume support for lower CPU use.

## Pause, resume, and cancel

Clone the handle before starting an operation, then use the clone from your UI or another task:

```rust
let download_control = handle.clone();
tokio::spawn(async move {
    // Choose the control action your application needs:
    download_control.pause();
    download_control.resume();
    download_control.cancel();
});
```

Pause takes effect at a worker checkpoint. Cancellation is permanent for that handle; create a new `DownloadHandle` for a later attempt. With `Full` verification, Irmin can reuse completed chunks after a process restart when the game, locale, install path, and manifest still match. It removes the resume state after success.

## Next steps

- Read the [`Sophon` API documentation](https://docs.rs/irmin_rs) for method signatures and error types.
- Use `irmin::load_download_state` or `irmin::state_file_path` to inspect persisted resume state.
- Use the lower-level `game_installer` module if you need direct control beyond `Sophon`.

## Development

Run the test suite with:

```sh
cargo test
```

Run benchmarks with `cargo bench --features benchmark`. The benchmark and memory diagnostic scripts are intended for library development, not application integration.

## License

BSD-3-Clause. The Elysiae Project.
