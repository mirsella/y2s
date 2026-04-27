use std::path::PathBuf;

use clap::Parser;

use crate::error::{AppError, Result};

#[derive(Debug, Parser)]
#[command(version, about = "Exact-mirror a public YouTube playlist into Spotify")]
pub struct Cli {
    /// Public YouTube or YouTube Music playlist URL, or a raw playlist ID.
    pub youtube_playlist_url: String,

    /// Explicit Spotify cookie file. Supports Netscape, JSON map, or raw Cookie header.
    #[arg(long, value_name = "COOKIE_FILE", conflicts_with = "browser_profile")]
    pub spotify_cookie_file: Option<PathBuf>,

    /// Filesystem path to the browser profile or cookie database to read Spotify cookies from.
    #[arg(
        long,
        value_name = "PROFILE_PATH",
        conflicts_with = "spotify_cookie_file",
        long_help = "Filesystem path to a browser profile directory or cookie database to read Spotify cookies from. Examples: ~/.config/google-chrome/Default, ~/.mozilla/firefox/<profile>.default-release, ~/.zen/<profile>, ~/.config/helium/Default, or a Chromium-style Cookies/Network/Cookies file. The path is tried with Chrome, Firefox, Zen, and Helium cookie readers."
    )]
    pub browser_profile: Option<PathBuf>,

    /// Spotify playlist name to sync into. Defaults to the YouTube playlist title.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Let opencode resolve ambiguous matches before prompting.
    ///
    /// y2s connects to an existing opencode server at http://127.0.0.1:4096,
    /// or starts one with `opencode serve` if none is running.
    #[arg(long, visible_alias = "oc")]
    pub opencode: bool,

    /// opencode model, e.g. `opencode/minimax-m2.5-free`.
    #[arg(
        long = "opencode-model",
        visible_alias = "oc-model",
        value_name = "MODEL"
    )]
    pub opencode_model: Option<String>,

    /// opencode variant to use for resolver prompts.
    #[arg(
        long = "opencode-variant",
        visible_alias = "oc-variant",
        value_name = "VARIANT"
    )]
    pub opencode_variant: Option<String>,

    /// Existing opencode server URL. Defaults to http://127.0.0.1:4096.
    #[arg(
        long = "opencode-base-url",
        visible_alias = "oc-base-url",
        value_name = "URL"
    )]
    pub opencode_base_url: Option<String>,

    /// Print the planned Spotify mutations without applying them.
    #[arg(long)]
    pub dry_run: bool,

    /// Maximum number of concurrent Spotify search requests.
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,

    /// Development/debug cap for the number of YouTube tracks to process.
    #[arg(long)]
    pub limit: Option<usize>,
}

impl Cli {
    pub fn validate(&self) -> Result<()> {
        if self.youtube_playlist_url.trim().is_empty() {
            return Err(AppError::InvalidInput(
                "YouTube playlist URL or ID cannot be empty".to_string(),
            ));
        }

        if self.concurrency == 0 {
            return Err(AppError::InvalidInput(
                "--concurrency must be at least 1".to_string(),
            ));
        }

        if self
            .name
            .as_deref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(AppError::InvalidInput("--name cannot be empty".to_string()));
        }

        for (flag, value) in [
            ("--opencode-model", self.opencode_model.as_deref()),
            ("--opencode-variant", self.opencode_variant.as_deref()),
            ("--opencode-base-url", self.opencode_base_url.as_deref()),
        ] {
            if value.is_some_and(|value| value.trim().is_empty()) {
                return Err(AppError::InvalidInput(format!("{flag} cannot be empty")));
            }
        }

        Ok(())
    }

    pub fn use_opencode(&self) -> bool {
        self.opencode
            || self.opencode_model.is_some()
            || self.opencode_variant.is_some()
            || self.opencode_base_url.is_some()
    }
}
