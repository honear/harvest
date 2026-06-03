# Harvest

Verified media ingest for macOS and Windows — safely copy footage and files
from a source (SD card, camera, project folder) to one or more destinations,
and **prove** the bytes arrived intact.

Inspired by [Hedge Offshoot](https://hedge.co/products/offshoot). Open source.

## Why

When you offload a card, "it copied without an error" is not the same as "every
byte is correct." Harvest reads the source once, writes to every destination,
forces the data to physical disk, then **reads each destination back and
compares checksums against the source**. If anything is wrong, you find out
immediately — not when you go to edit the footage months later.

## Status

Early development. The core engine and a CLI front end work today; a Tauri
desktop GUI is planned.

### Working now
- Verified copy with full read-back verification (toggleable)
- One source → multiple destinations in a single read
- xxHash3 (fast, default) or MD5 (interop with media-hash tooling)
- Parallel across files; live progress and throughput

### Planned
- Stop & resume (transfer journal)
- Media Hash List (MHL) / sidecar manifest output
- Filters (extension / size / date) and rename-on-ingest
- Presets that build dated destination folder structures
- Cross-project archive de-duplication with reflink/hardlink linking
- Two-pane compare & conflict resolution

## Layout

```
crates/
  harvest-core/   UI-agnostic engine: hashing, verified copy, scanning
  harvest-cli/    `harvest` command-line front end
example/          FreeFileSync source — reference for compare/sync algorithms
```

## Build & run

Requires a [Rust](https://rustup.rs) toolchain.

```sh
cargo build --release
cargo test

# Copy a folder to two backup drives, verifying each:
harvest copy /path/to/SDCARD /Volumes/BackupA /Volumes/BackupB --hash xxh3
```

## License

MIT — see [LICENSE](LICENSE).
