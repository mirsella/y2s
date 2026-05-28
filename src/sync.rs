use crate::{error::Result, model::PlaylistSnapshot, progress::Progress, spotify::SpotifyClient};

#[derive(Debug)]
pub struct SyncPlan {
    playlist_uri: String,
    remove_uids: Vec<String>,
    add_uris: Vec<String>,
}

impl SyncPlan {
    pub fn is_noop(&self) -> bool {
        self.remove_uids.is_empty() && self.add_uris.is_empty()
    }

    pub fn removed_count(&self) -> usize {
        self.remove_uids.len()
    }

    pub fn added_count(&self) -> usize {
        self.add_uris.len()
    }
}

pub fn plan_exact_mirror(snapshot: &PlaylistSnapshot, desired_uris: &[String]) -> SyncPlan {
    let mut retained = vec![false; snapshot.items.len()];
    let mut next_search_index = 0;
    let mut kept_prefix_len = 0;

    // With only remove-by-UID and append operations, the stable base is the
    // longest desired prefix already present as an ordered subsequence.
    for desired_uri in desired_uris {
        let Some(offset) = snapshot.items[next_search_index..]
            .iter()
            .position(|item| item.track.uri == *desired_uri)
        else {
            break;
        };

        let current_index = next_search_index + offset;
        retained[current_index] = true;
        next_search_index = current_index + 1;
        kept_prefix_len += 1;
    }

    SyncPlan {
        playlist_uri: snapshot.uri.clone(),
        remove_uids: snapshot
            .items
            .iter()
            .enumerate()
            .filter(|(index, _)| !retained[*index])
            .map(|(_, item)| item.uid.clone())
            .collect(),
        add_uris: desired_uris[kept_prefix_len..].to_vec(),
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

    fn plan(current: &[&str], desired: &[&str]) -> SyncPlan {
        let desired = desired
            .iter()
            .map(|uri| (*uri).to_string())
            .collect::<Vec<_>>();
        plan_exact_mirror(&snapshot(current), &desired)
    }

    #[test]
    fn identical_playlist_is_noop() {
        let plan = plan(&["a", "b"], &["a", "b"]);
        assert!(plan.is_noop());
        assert!(plan.remove_uids.is_empty());
    }

    #[test]
    fn deleted_tracks_are_removed_without_rebuilding() {
        let plan = plan(&["a", "skip", "b", "c", "skip-2"], &["a", "b", "c"]);
        assert!(!plan.is_noop());
        assert_eq!(plan.remove_uids, vec!["uid-1", "uid-4"]);
        assert!(plan.add_uris.is_empty());
    }

    #[test]
    fn middle_insert_rebuilds_only_the_suffix() {
        let plan = plan(&["a", "c", "d"], &["a", "b", "c", "d"]);
        assert_eq!(plan.remove_uids, vec!["uid-1", "uid-2"]);
        assert_eq!(plan.add_uris, vec!["b", "c", "d"]);
    }

    #[test]
    fn reordered_playlist_moves_earlier_tracks_to_the_bottom() {
        let plan = plan(&["a", "b"], &["b", "a"]);
        assert!(!plan.is_noop());
        assert_eq!(plan.remove_uids, vec!["uid-0"]);
        assert_eq!(plan.add_uris, vec!["a"]);
    }

    #[test]
    fn duplicate_tracks_are_preserved_in_desired_order() {
        let plan = plan(&["a"], &["a", "a"]);
        assert!(plan.remove_uids.is_empty());
        assert_eq!(plan.add_uris, vec!["a"]);
    }
}
