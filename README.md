# 🌱 Harvest

Verified media ingest for macOS and Windows — safely copy footage and files
from a source (SD card, camera, project folder) to one or more destinations,
and **prove** the bytes arrived intact.

Inspired by [Hedge Offshoot](https://hedge.co/products/offshoot). Open source (MIT).

## Why

When you offload a card, "it copied without an error" is not the same as "every
byte is correct." Harvest reads the source once, writes to every destination,
forces the data to physical disk, then **reads each destination back and
compares checksums against the source**. If anything is wrong, you find out
immediately — not when you go to edit the footage months later.

## Features

- **Verified copy** — read-back verification of every byte (toggleable).
- **One source → many destinations** in a single read (fan-out).
- **Checksums:** xxHash64 (fast, MHL-standard default), xxHash3, or MD5.
- **Skip / incremental** — re-runs skip files already present (path + size +
  mtime); source modification times are preserved.
- **Stop & resume** via a crash-safe transfer journal.
- **Pre-flight compare** — before copying, see what's new / already there /
  differs, with a free-space check.
- **Filters** — include/exclude extensions, size and date ranges, owner, and a
  managed exclude list.
- **Folder templates** — organize on ingest, e.g. `{project}/{YYYY}-{MM}-{DD}/{filename}`.
- **Manifests** — Media Hash List (MHL) or sidecar proof-of-transfer.
- **Sow & Survey** — a treemap visualizer to explore a source and exclude files
  by clicking (Sow), or survey any drive's disk usage (Survey).
- **Saved transfers** (presets), transfer history, completion notifications,
  keep-awake, and auto-eject of removable sources after a verified copy.

## Layout

```
crates/
  harvest-core/   UI-agnostic engine: hashing, verified copy, scan, filter,
                  template, journal, manifest, plan/verify orchestration
  harvest-cli/    `harvest` command-line front end
src/              desktop UI (TypeScript + Vite)
src-tauri/        Tauri (Rust) backend for the desktop app
example/          FreeFileSync source — local reference only (not built)
```

## Develop

Requires [Rust](https://rustup.rs) and [Node](https://nodejs.org).

```sh
npm install
npm run tauri dev      # run the desktop app
npm run build          # type-check + build the frontend
cargo test             # engine tests
```

### CLI

```sh
cargo build --release
# Copy a folder to two backup drives, verifying each:
harvest copy /path/to/SDCARD /Volumes/BackupA /Volumes/BackupB --hash xxh64
```

## License

MIT — see [LICENSE](LICENSE).
