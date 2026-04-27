use crate::{
    error::Result,
    model::{PlaylistSnapshot, SyncPlan},
    progress::Progress,
    spotify::SpotifyClient,
};

pub fn plan_exact_mirror(snapshot: &PlaylistSnapshot, desired_uris: Vec<String>) -> SyncPlan {
    let current_uris = snapshot
        .items
        .iter()
        .map(|item| item.track.uri.clone())
        .collect::<Vec<_>>();
    let changed = current_uris != desired_uris;

    SyncPlan {
        playlist_uri: snapshot.uri.clone(),
        current_uris,
        remove_uids: if changed {
            snapshot.items.iter().map(|item| item.uid.clone()).collect()
        } else {
            Vec::new()
        },
        add_uris: if changed {
            desired_uris.clone()
        } else {
            Vec::new()
        },
        desired_uris,
    }
}

pub async fn execute_plan(
    spotify: &SpotifyClient,
    plan: &SyncPlan,
    dry_run: bool,
    progress: &Progress,
) -> Result<()> {
    if plan.is_noop() || dry_run {
        return Ok(());
    }

    if !plan.remove_uids.is_empty() {
        let remove_bar =
            progress.track_bar(plan.remove_uids.len(), "removing Spotify extras/order");
        for chunk in plan.remove_uids.chunks(100) {
            spotify
                .remove_playlist_items(&plan.playlist_uri, chunk)
                .await?;
            remove_bar.inc(chunk.len() as u64);
        }
        remove_bar.finish_and_clear();
    }

    if !plan.add_uris.is_empty() {
        let add_bar = progress.track_bar(
            plan.add_uris.len(),
            "adding Spotify tracks in YouTube order",
        );
        for chunk in plan.add_uris.chunks(100) {
            spotify
                .add_tracks_to_playlist(&plan.playlist_uri, chunk)
                .await?;
            add_bar.inc(chunk.len() as u64);
        }
        add_bar.finish_and_clear();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::model::{PlaylistSnapshot, SpotifyPlaylistItem, SpotifyTrack};

    use super::*;

    fn track(uri: &str) -> SpotifyTrack {
        SpotifyTrack {
            uri: uri.to_string(),
            title: uri.to_string(),
            artists: Vec::new(),
            album: None,
            duration_ms: None,
            image_url: None,
        }
    }

    fn snapshot(uris: &[&str]) -> PlaylistSnapshot {
        PlaylistSnapshot {
            uri: "spotify:playlist:p".to_string(),
            name: "P".to_string(),
            items: uris
                .iter()
                .enumerate()
                .map(|(idx, uri)| SpotifyPlaylistItem {
                    uid: format!("uid-{idx}"),
                    track: track(uri),
                })
                .collect(),
        }
    }

    #[test]
    fn identical_playlist_is_noop() {
        let plan = plan_exact_mirror(
            &snapshot(&["a", "b"]),
            vec!["a".to_string(), "b".to_string()],
        );
        assert!(plan.is_noop());
        assert!(plan.remove_uids.is_empty());
    }

    #[test]
    fn reordered_playlist_rebuilds() {
        let plan = plan_exact_mirror(
            &snapshot(&["a", "b"]),
            vec!["b".to_string(), "a".to_string()],
        );
        assert!(!plan.is_noop());
        assert_eq!(plan.remove_uids, vec!["uid-0", "uid-1"]);
        assert_eq!(plan.add_uris, vec!["b", "a"]);
    }

    #[test]
    fn duplicate_tracks_are_preserved_in_desired_order() {
        let plan = plan_exact_mirror(&snapshot(&["a"]), vec!["a".to_string(), "a".to_string()]);
        assert_eq!(plan.add_uris, vec!["a", "a"]);
    }
}
