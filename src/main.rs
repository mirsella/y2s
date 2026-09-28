mod cli;
mod cookies;
mod error;
mod matching;
mod model;
mod opencode;
mod progress;
mod report;
mod spotify;
mod sync;
mod youtube;

use clap::Parser;
use cli::Cli;
use error::Result;
use model::{PlaylistSnapshot, SpotifyPlaylistSummary};
use progress::Progress;
use spotify::SpotifyClient;

struct SpotifyState {
    client: SpotifyClient,
    playlist: SpotifyPlaylistSummary,
    current: PlaylistSnapshot,
    playlist_would_be_created: bool,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();
    cli.validate()?;

    run(cli).await?;
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let progress = Progress::new();

    progress.set_phase("loading YouTube playlist and Spotify session");
    let explicit_name = cli.name.as_deref().map(str::trim);
    let (youtube_playlist, spotify) = tokio::try_join!(
        youtube::fetch_playlist(&cli.youtube_playlist_url, cli.limit, &progress),
        connect_spotify(&cli, &progress)
    )?;

    let playlist_name = explicit_name.unwrap_or(&youtube_playlist.title);
    progress.set_phase(if cli.use_opencode() {
        "loading Spotify playlist and opencode resolver"
    } else {
        "loading Spotify playlist"
    });
    let (spotify_state, opencode) = tokio::try_join!(
        load_spotify_playlist(&progress, spotify, playlist_name, cli.dry_run),
        connect_opencode(&cli, &progress)
    )?;

    let SpotifyState {
        client: spotify,
        playlist: spotify_playlist,
        current,
        playlist_would_be_created,
    } = spotify_state;

    progress.set_phase("searching and matching Spotify tracks");
    let matches = matching::resolve_playlist(
        &spotify,
        &youtube_playlist.tracks,
        &current,
        cli.concurrency,
        opencode.as_ref(),
        &progress,
    )
    .await?;

    let desired_uris = matches
        .matched
        .iter()
        .map(|matched| matched.spotify.uri.clone())
        .collect::<Vec<_>>();
    let plan = sync::plan_exact_mirror(&current, desired_uris);

    progress.set_phase(if cli.dry_run {
        "building dry-run summary"
    } else {
        "syncing Spotify playlist"
    });
    sync::execute_plan(&spotify, &plan, cli.dry_run, &progress).await?;
    progress.finish();

    print!(
        "{}",
        report::format_report(
            &youtube_playlist,
            &spotify_playlist,
            &matches,
            &plan,
            cli.dry_run,
            playlist_would_be_created,
        )
    );

    Ok(())
}

async fn connect_spotify(cli: &Cli, progress: &Progress) -> Result<SpotifyClient> {
    let cookies = cookies::load_cookies(cli, progress).await?;
    let spinner = progress.spinner("authenticating Spotify web session");
    let spotify = SpotifyClient::connect(cookies).await;
    spinner.finish_and_clear();
    spotify
}

async fn connect_opencode(
    cli: &Cli,
    progress: &Progress,
) -> Result<Option<opencode::OpencodeResolver>> {
    if !cli.use_opencode() {
        return Ok(None);
    }

    let spinner = progress.spinner("connecting opencode resolver");
    let resolver = opencode::OpencodeResolver::connect(opencode::OpencodeConfig::new(
        cli.opencode_base_url.clone(),
        cli.opencode_model.clone(),
        cli.opencode_variant.clone(),
    ))
    .await;
    spinner.finish_and_clear();
    resolver.map(Some)
}

async fn load_spotify_playlist(
    progress: &Progress,
    spotify: SpotifyClient,
    playlist_name: &str,
    dry_run: bool,
) -> Result<SpotifyState> {
    let spinner = progress.spinner(format!("finding Spotify playlist {playlist_name:?}"));
    let playlist = spotify.find_playlist_by_name(playlist_name).await?;
    spinner.finish_and_clear();

    let (playlist, current, playlist_would_be_created) = match playlist {
        Some(playlist) => {
            let spinner = progress.spinner(format!(
                "loading current Spotify playlist {playlist_name:?}"
            ));
            let current = spotify.fetch_playlist(&playlist.uri).await?;
            spinner.finish_and_clear();
            (playlist, current, false)
        }
        None if dry_run => synthetic_playlist(playlist_name),
        None => {
            let spinner = progress.spinner(format!("creating Spotify playlist {playlist_name:?}"));
            let playlist = spotify.create_playlist(playlist_name).await?;
            spinner.finish_and_clear();
            let current = empty_snapshot(&playlist);
            (playlist, current, false)
        }
    };

    Ok(SpotifyState {
        client: spotify,
        playlist,
        current,
        playlist_would_be_created,
    })
}

fn synthetic_playlist(name: &str) -> (SpotifyPlaylistSummary, PlaylistSnapshot, bool) {
    let playlist = SpotifyPlaylistSummary {
        uri: format!("<new Spotify playlist: {name}>"),
        name: name.to_string(),
    };
    let current = empty_snapshot(&playlist);
    (playlist, current, true)
}

fn empty_snapshot(playlist: &SpotifyPlaylistSummary) -> PlaylistSnapshot {
    PlaylistSnapshot {
        uri: playlist.uri.clone(),
        name: playlist.name.clone(),
        items: Vec::new(),
    }
}
