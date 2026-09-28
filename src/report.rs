use std::fmt::Write;

use crate::{
    model::{MatchResult, MatchSource, MatchedTrack, SpotifyPlaylistSummary, YoutubePlaylist},
    sync::SyncPlan,
};

pub fn format_report(
    youtube: &YoutubePlaylist,
    spotify: &SpotifyPlaylistSummary,
    matches: &MatchResult,
    plan: &SyncPlan,
    dry_run: bool,
    playlist_would_be_created: bool,
) -> String {
    let mut out = String::new();
    let without_opencode = matches
        .matched
        .iter()
        .filter(|matched| matches!(matched.source, MatchSource::WithoutOpencode))
        .count();
    let with_opencode = matches.matched.len() - without_opencode;

    writeln!(out, "Playlist: {}", youtube.title).unwrap();
    writeln!(
        out,
        "YouTube: {} ({} tracks)",
        youtube.id,
        youtube.tracks.len()
    )
    .unwrap();
    if playlist_would_be_created {
        writeln!(out, "Spotify: {} (would be created)", spotify.name).unwrap();
    } else {
        writeln!(out, "Spotify: {} ({})", spotify.name, spotify.uri).unwrap();
        if let Some(id) = spotify.uri.strip_prefix("spotify:playlist:")
            && !id.is_empty()
        {
            writeln!(out, "Link: https://open.spotify.com/playlist/{id}").unwrap();
        }
    }

    writeln!(
        out,
        "Matched: {}/{} ({} without opencode, {} with opencode)",
        matches.matched.len(),
        youtube.tracks.len(),
        without_opencode,
        with_opencode
    )
    .unwrap();
    writeln!(out, "Missed: {}", matches.skipped.len()).unwrap();
    if !matches.matched.is_empty() {
        let average_score = matches
            .matched
            .iter()
            .map(|matched| matched.score)
            .sum::<f64>()
            / matches.matched.len() as f64;
        writeln!(out, "Average match score: {average_score:.1}").unwrap();
    }

    if plan.is_noop() {
        writeln!(out, "Sync: already exact").unwrap();
    } else if dry_run {
        writeln!(
            out,
            "Dry run: would remove {} entries and add {} entries",
            plan.removed_count(),
            plan.added_count()
        )
        .unwrap();
    } else {
        writeln!(
            out,
            "Sync applied: removed {} entries and added {} entries",
            plan.removed_count(),
            plan.added_count()
        )
        .unwrap();
    }

    writeln!(out, "\nMatched without opencode ({without_opencode}):").unwrap();
    for matched in matches
        .matched
        .iter()
        .filter(|matched| matches!(matched.source, MatchSource::WithoutOpencode))
    {
        write_match(&mut out, matched);
    }

    writeln!(out, "\nMatched with opencode ({with_opencode}):").unwrap();
    for matched in matches
        .matched
        .iter()
        .filter(|matched| matches!(matched.source, MatchSource::Opencode { .. }))
    {
        write_match(&mut out, matched);
        if let MatchSource::Opencode {
            reason: Some(reason),
        } = &matched.source
            && !reason.trim().is_empty()
        {
            writeln!(out, "      Reason: {}", reason.trim()).unwrap();
        }
    }

    writeln!(out, "\nMissed ({}):", matches.skipped.len()).unwrap();
    for skipped in &matches.skipped {
        writeln!(
            out,
            "  #{} {} - {}",
            skipped.youtube.index + 1,
            skipped.youtube.artist_display(),
            skipped.youtube.title
        )
        .unwrap();
        writeln!(out, "      Reason: {}", skipped.reason).unwrap();
    }

    out
}

fn write_match(out: &mut String, matched: &MatchedTrack) {
    writeln!(
        out,
        "  #{} {} - {} -> {} - {} ({})",
        matched.youtube.index + 1,
        matched.youtube.artist_display(),
        matched.youtube.title,
        matched.spotify.artist_display(),
        matched.spotify.title,
        matched.spotify.uri
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{MatchedTrack, SkippedTrack, SpotifyTrack, YoutubeTrack},
        sync::plan_exact_mirror,
    };

    use super::*;

    fn youtube_track(index: usize) -> YoutubeTrack {
        YoutubeTrack {
            index,
            title: format!("Song {index}"),
            artists: vec!["Artist".into()],
            album: None,
            duration_ms: None,
            video_id: format!("video-{index}"),
            thumbnails: vec![],
        }
    }

    #[test]
    fn report_separates_each_match_and_miss() {
        let youtube = YoutubePlaylist {
            id: "yt-id".into(),
            title: "Music".into(),
            tracks: (0..3).map(youtube_track).collect(),
        };
        let spotify = SpotifyPlaylistSummary {
            uri: "spotify:playlist:abc".into(),
            name: "Music".into(),
        };
        let matched = |index, source| MatchedTrack {
            youtube: youtube_track(index),
            spotify: SpotifyTrack {
                uri: format!("spotify:track:{index}"),
                title: format!("Song {index}"),
                artists: vec!["Artist".into()],
                album: None,
                duration_ms: None,
                image_url: None,
            },
            score: 90.0,
            source,
        };
        let matches = MatchResult {
            matched: vec![
                matched(0, MatchSource::WithoutOpencode),
                matched(
                    1,
                    MatchSource::Opencode {
                        reason: Some("Album version".into()),
                    },
                ),
            ],
            skipped: vec![SkippedTrack {
                youtube: youtube_track(2),
                reason: "opencode: no match".into(),
            }],
        };
        let snapshot = crate::model::PlaylistSnapshot {
            uri: spotify.uri.clone(),
            name: spotify.name.clone(),
            items: vec![],
        };
        let plan = plan_exact_mirror(
            &snapshot,
            matches
                .matched
                .iter()
                .map(|m| m.spotify.uri.clone())
                .collect(),
        );

        let report = format_report(&youtube, &spotify, &matches, &plan, true, false);
        assert!(report.contains("Matched: 2/3 (1 without opencode, 1 with opencode)\nMissed: 1"));
        assert!(report.contains("Link: https://open.spotify.com/playlist/abc"));
        assert!(report.contains("Dry run: would remove 0 entries and add 2 entries"));
        assert!(report.contains("Matched without opencode (1):\n  #1 Artist - Song 0 ->"));
        assert!(report.contains("Matched with opencode (1):\n  #2 Artist - Song 1 ->"));
        assert!(report.contains("Reason: Album version"));
        assert!(
            report.contains("Missed (1):\n  #3 Artist - Song 2\n      Reason: opencode: no match")
        );
        for index in 1..=3 {
            assert_eq!(
                report.matches(&format!("  #{index} Artist - Song")).count(),
                1
            );
        }
    }
}
