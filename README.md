<div align="center">

# y2s

**Exact-mirror public YouTube and YouTube Music playlists into Spotify.**

[![Rust CI](https://github.com/mirsella/y2s/actions/workflows/rust.yml/badge.svg)](https://github.com/mirsella/y2s/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-111827.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024-f97316.svg)](Cargo.toml)
[![MSRV 1.85](https://img.shields.io/badge/MSRV-1.85-2563eb.svg)](Cargo.toml)

`y2s` turns a public YouTube playlist into a Spotify playlist with the same order, the same contents, and no silent drift.

</div>

## Why

Spotify and YouTube do not describe the same catalog in the same way. Titles are noisy, artists are reordered, releases differ by region, and official playlist mutation APIs often add product restrictions that are painful for personal tooling.

`y2s` is built as a sharp local CLI instead:

- Fetches public YouTube and YouTube Music playlists through Innertube.
- Authenticates to Spotify with your existing browser session cookies.
- Searches Spotify with scoring tuned for playlist migration.
- Reuses tracks already present in the target playlist when possible.
- Prompts only when the match is genuinely ambiguous.
- Optionally asks `opencode` to resolve ambiguous matches before falling back to manual input.
- Applies an exact mirror plan so the Spotify playlist ends in the desired order.

## Install

```bash
cargo install --git https://github.com/mirsella/y2s.git
```

For local development:

```bash
git clone https://github.com/mirsella/y2s.git
cd y2s
cargo build --release
```

## Quick Start

```bash
y2s 'https://music.youtube.com/playlist?list=PLxxxxxxxxxxxxxxxx'
```

By default, the Spotify playlist name is copied from the YouTube playlist title. If it does not exist, `y2s` creates it unless `--dry-run` is enabled.

Use an explicit target playlist name:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --name 'Roadtrip Mirror'
```

Preview without changing Spotify:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --dry-run
```

## Spotify Login

`y2s` uses Spotify web session cookies from your browser. You can either let it read a browser profile directly:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --browser-profile ~/.config/google-chrome/Default
```

Or pass a cookie export file:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --spotify-cookie-file ./spotify-cookies.txt
```

Supported cookie input formats include Netscape cookies, JSON maps, and raw `Cookie` headers.

## Ambiguous Matches

When scoring cannot safely choose a Spotify result, `y2s` pauses progress rendering and asks you to pick, skip, or enter another Spotify URI.

To let `opencode` try first:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --opencode
```

Optional resolver configuration:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' \
  --opencode-model 'opencode/minimax-m2.5-free' \
  --opencode-variant default \
  --opencode-base-url http://127.0.0.1:4096
```

## CLI

```text
Usage: y2s [OPTIONS] <YOUTUBE_PLAYLIST_URL>

Arguments:
  <YOUTUBE_PLAYLIST_URL>  Public YouTube or YouTube Music playlist URL, or a raw playlist ID

Options:
      --spotify-cookie-file <COOKIE_FILE>  Explicit Spotify cookie file
      --browser-profile <PROFILE_PATH>     Browser profile or cookie database to read Spotify cookies from
      --name <NAME>                         Spotify playlist name to sync into
      --opencode                            Let opencode resolve ambiguous matches before prompting
      --opencode-model <MODEL>              opencode model
      --opencode-variant <VARIANT>          opencode variant
      --opencode-base-url <URL>             Existing opencode server URL
      --dry-run                             Print planned Spotify mutations without applying them
      --concurrency <CONCURRENCY>           Maximum concurrent Spotify search requests [default: 8]
      --limit <LIMIT>                       Development/debug cap for tracks to process
  -h, --help                                Print help
  -V, --version                             Print version
```

## Development

```bash
cargo fmt --all
cargo check --all-features
cargo clippy --all-features -- -D warnings
cargo test --all-features
```

The CI workflow runs the same formatting, compilation, lint, and test gates on every push and pull request to `main`.

## License

MIT. See [`LICENSE`](LICENSE).
