use crate::{
    error::{AppError, Result},
    model::PlaylistSnapshot,
    progress::Progress,
    spotify::SpotifyClient,
};

#[derive(Debug, PartialEq, Eq)]
struct AddRun {
    after_uid: Option<String>,
    uris: Vec<String>,
}

#[derive(Debug)]
pub struct SyncPlan {
    playlist_uri: String,
    remove_uids: Vec<String>,
    add_runs: Vec<AddRun>,
    desired_uris: Vec<String>,
}

impl SyncPlan {
    pub fn is_noop(&self) -> bool {
        self.remove_uids.is_empty() && self.add_runs.is_empty()
    }

    pub fn removed_count(&self) -> usize {
        self.remove_uids.len()
    }

    pub fn added_count(&self) -> usize {
        self.add_runs.iter().map(|run| run.uris.len()).sum()
    }
}

pub fn plan_exact_mirror(snapshot: &PlaylistSnapshot, desired_uris: Vec<String>) -> SyncPlan {
    let retained_pairs = longest_common_subsequence(snapshot, &desired_uris);

    let mut retained = vec![false; snapshot.items.len()];
    for (current_index, _) in &retained_pairs {
        retained[*current_index] = true;
    }

    let mut add_runs = Vec::new();
    let mut next_desired_index = 0;
    let mut after_uid = None;
    for (current_index, desired_index) in retained_pairs {
        if next_desired_index < desired_index {
            add_runs.push(AddRun {
                after_uid: after_uid.clone(),
                uris: desired_uris[next_desired_index..desired_index].to_vec(),
            });
        }
        next_desired_index = desired_index + 1;
        after_uid = Some(snapshot.items[current_index].uid.clone());
    }
    if next_desired_index < desired_uris.len() {
        add_runs.push(AddRun {
            after_uid,
            uris: desired_uris[next_desired_index..].to_vec(),
        });
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
        add_runs,
        desired_uris,
    }
}

fn longest_common_subsequence(
    snapshot: &PlaylistSnapshot,
    desired_uris: &[String],
) -> Vec<(usize, usize)> {
    let current_len = snapshot.items.len();
    let desired_len = desired_uris.len();
    let mut lengths = vec![vec![0usize; desired_len + 1]; current_len + 1];

    for current_index in (0..current_len).rev() {
        for desired_index in (0..desired_len).rev() {
            lengths[current_index][desired_index] =
                if snapshot.items[current_index].track.uri == desired_uris[desired_index] {
                    lengths[current_index + 1][desired_index + 1] + 1
                } else {
                    lengths[current_index + 1][desired_index]
                        .max(lengths[current_index][desired_index + 1])
                };
        }
    }

    let mut pairs = Vec::with_capacity(lengths[0][0]);
    let mut current_index = 0;
    let mut desired_index = 0;
    while current_index < current_len && desired_index < desired_len {
        if snapshot.items[current_index].track.uri == desired_uris[desired_index] {
            pairs.push((current_index, desired_index));
            current_index += 1;
            desired_index += 1;
        } else if lengths[current_index + 1][desired_index]
            > lengths[current_index][desired_index + 1]
        {
            current_index += 1;
        } else {
            desired_index += 1;
        }
    }
    pairs
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
        spotify
            .remove_playlist_items(&plan.playlist_uri, &plan.remove_uids)
            .await?;
        remove_bar.inc(plan.remove_uids.len() as u64);
        remove_bar.finish_and_clear();
    }

    if !plan.add_runs.is_empty() {
        let add_bar =
            progress.track_bar(plan.added_count(), "adding Spotify tracks in YouTube order");

        for run in &plan.add_runs {
            spotify
                .insert_playlist_items(&plan.playlist_uri, &run.uris, run.after_uid.as_deref())
                .await?;
            add_bar.inc(run.uris.len() as u64);
        }
        add_bar.finish_and_clear();
    }

    let actual = spotify.fetch_playlist(&plan.playlist_uri).await?;
    let mismatch = actual
        .items
        .iter()
        .map(|item| &item.track.uri)
        .zip(&plan.desired_uris)
        .position(|(actual, expected)| actual != expected);
    if mismatch.is_some() || actual.items.len() != plan.desired_uris.len() {
        let mismatch = mismatch.unwrap_or_else(|| actual.items.len().min(plan.desired_uris.len()));
        return Err(AppError::Spotify(format!(
            "playlist verification failed at position {}: expected {} entries, found {}",
            mismatch + 1,
            plan.desired_uris.len(),
            actual.items.len()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::model::{PlaylistSnapshot, SpotifyPlaylistItem, SpotifyTrack};

    use super::*;

    fn snapshot(uris: &[&str]) -> PlaylistSnapshot {
        PlaylistSnapshot {
            uri: "spotify:playlist:p".to_string(),
            name: "P".to_string(),
            items: uris
                .iter()
                .enumerate()
                .map(|(idx, uri)| SpotifyPlaylistItem {
                    uid: format!("uid-{idx}"),
                    track: SpotifyTrack {
                        uri: uri.to_string(),
                        title: uri.to_string(),
                        artists: Vec::new(),
                        album: None,
                        duration_ms: None,
                        image_url: None,
                    },
                })
                .collect(),
        }
    }

    fn plan(current: &[&str], desired: &[&str]) -> SyncPlan {
        let desired = desired
            .iter()
            .map(|uri| (*uri).to_string())
            .collect::<Vec<_>>();
        plan_exact_mirror(&snapshot(current), desired)
    }

    fn assert_plan(
        current: &[&str],
        desired: &[&str],
        removed: &[&str],
        additions: &[(Option<&str>, &[&str])],
    ) {
        let plan = plan(current, desired);
        let additions = additions
            .iter()
            .map(|(after_uid, uris)| AddRun {
                after_uid: after_uid.map(str::to_string),
                uris: uris.iter().map(|uri| uri.to_string()).collect(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            plan.remove_uids
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            removed
        );
        assert_eq!(plan.add_runs, additions);
    }

    #[test]
    fn identical_playlist_is_noop() {
        let plan = plan(&["a", "b"], &["a", "b"]);
        assert!(plan.is_noop());
        assert!(plan.remove_uids.is_empty());
    }

    #[test]
    fn deleted_tracks_are_removed_without_rebuilding() {
        assert_plan(
            &["a", "skip", "b", "c", "skip-2"],
            &["a", "b", "c"],
            &["uid-1", "uid-4"],
            &[],
        );
    }

    #[test]
    fn front_insert_adds_only_the_missing_prefix() {
        assert_plan(&["b", "c"], &["a", "b", "c"], &[], &[(None, &["a"])]);
    }

    #[test]
    fn empty_playlist_adds_everything_at_the_top() {
        assert_plan(&[], &["a", "b"], &[], &[(None, &["a", "b"])]);
    }

    #[test]
    fn middle_insert_retains_both_sides() {
        assert_plan(
            &["a", "c", "d"],
            &["a", "b", "c", "d"],
            &[],
            &[(Some("uid-0"), &["b"])],
        );
    }

    #[test]
    fn missing_ends_are_added_around_the_retained_block() {
        assert_plan(
            &["b", "c"],
            &["a", "b", "c", "d"],
            &[],
            &[(None, &["a"]), (Some("uid-1"), &["d"])],
        );
    }

    #[test]
    fn multiple_middle_gaps_retain_all_ordered_tracks() {
        assert_plan(
            &["a", "b", "c", "d"],
            &["a", "x", "b", "c", "y", "d"],
            &[],
            &[(Some("uid-0"), &["x"]), (Some("uid-2"), &["y"])],
        );
    }

    #[test]
    fn replacement_removes_only_changed_occurrence() {
        assert_plan(
            &["a", "old", "c"],
            &["a", "new", "c"],
            &["uid-1"],
            &[(Some("uid-0"), &["new"])],
        );
    }

    #[test]
    fn reordered_playlist_retains_one_lcs_occurrence() {
        assert_plan(&["a", "b"], &["b", "a"], &["uid-1"], &[(None, &["b"])]);
    }

    #[test]
    fn duplicate_tracks_are_preserved_in_desired_order() {
        assert_plan(&["a"], &["a", "a"], &[], &[(Some("uid-0"), &["a"])]);
    }

    #[test]
    fn extra_duplicate_is_removed_by_occurrence_uid() {
        assert_plan(&["a", "a", "b"], &["a", "b"], &["uid-1"], &[]);
    }
}
