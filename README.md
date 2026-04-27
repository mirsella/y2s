<div align="center">

# y2s

**Mirror YouTube playlists to Spotify.**

[![Rust CI](https://github.com/mirsella/y2s/actions/workflows/rust.yml/badge.svg)](https://github.com/mirsella/y2s/actions/workflows/rust.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-111827.svg)](LICENSE)
[![Rust 2024](https://img.shields.io/badge/rust-2024-f97316.svg)](Cargo.toml)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-2563eb.svg)](Cargo.toml)

`y2s` turns a public YouTube playlist into a Spotify playlist with the same order, the same contents, and no silent drift.

</div>

## Why

Spotify and YouTube do not describe the same catalog in the same way. Titles are noisy, artists are reordered, releases differ by region, and official playlist mutation APIs often add product restrictions that are painful for personal tooling.

`y2s` is built as a sharp local CLI instead:

- Fetches public YouTube playlists through Innertube.
- Authenticates to Spotify with your existing browser session cookies.
- Searches Spotify with scoring tuned for playlist migration.
- Reuses tracks already present in the target playlist when possible.
- Prompts only when the match is genuinely ambiguous.
- Optionally lets `opencode` resolve ambiguous matches automatically, skipping unresolved tracks.
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

`y2s` uses your existing Spotify web session from a logged-in browser. In the normal case, there is nothing to configure: run the command and let `y2s` auto-detect usable Spotify cookies from supported browser profiles.

```bash
y2s 'PLxxxxxxxxxxxxxxxx'
```

If you use multiple browser profiles or keep cookies in a non-standard location, point `y2s` at the profile directly:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --browser-profile ~/.config/google-chrome/Default
```

As a last resort, pass a cookie export file:

```bash
y2s 'PLxxxxxxxxxxxxxxxx' --spotify-cookie-file ./spotify-cookies.txt
```

Supported cookie input formats include Netscape cookies, JSON maps, and raw `Cookie` headers.

## Ambiguous Matches

When scoring cannot safely choose a Spotify result, `y2s` pauses progress rendering and asks you to pick, skip, or enter another Spotify URI.

With `--opencode`, this becomes non-interactive: `opencode` chooses a candidate or `y2s` skips the track.

To make ambiguous resolution fully automatic with `opencode`:

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
  <YOUTUBE_PLAYLIST_URL>  Public YouTube playlist URL, or a raw playlist ID

Options:
      --spotify-cookie-file <COOKIE_FILE>  Explicit Spotify cookie file
      --browser-profile <PROFILE_PATH>     Browser profile or cookie database to read Spotify cookies from
      --name <NAME>                         Spotify playlist name to sync into
      --opencode                            Let opencode resolve ambiguous matches automatically, skipping unresolved tracks
      --opencode-model <MODEL>              opencode model
      --opencode-variant <VARIANT>          opencode variant
      --opencode-base-url <URL>             Existing opencode server URL
      --dry-run                             Print planned Spotify mutations without applying them
      --concurrency <CONCURRENCY>           Maximum tracks to search on Spotify at once [default: 10]
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
