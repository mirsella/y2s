use std::{
    collections::HashSet,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

use crate::{
    error::{AppError, Result, body_excerpt},
    model::{PlaylistSnapshot, SpotifyPlaylistItem, SpotifyPlaylistSummary},
    spotify::{SpotifyClient, search},
};

// Larger private Pathfinder additions have been observed returning successful no-ops.
const ADD_BATCH_SIZE: usize = 25;

impl SpotifyClient {
    pub async fn find_playlist_by_name(
        &self,
        name: &str,
    ) -> Result<Option<SpotifyPlaylistSummary>> {
        let variables = json!({
            "filters": ["Playlists"],
            "order": null,
            "textFilter": "",
            "features": ["LIKED_SONGS", "YOUR_EPISODES"],
            "limit": 250,
            "offset": 0,
        });
        let response = self.graph_query("libraryV3", variables).await?;
        let mut playlists = Vec::new();
        collect_playlists(&response, &mut playlists);
        Ok(playlists.into_iter().find(|playlist| playlist.name == name))
    }

    pub async fn fetch_playlist(&self, playlist_uri: &str) -> Result<PlaylistSnapshot> {
        let mut items = Vec::new();
        let mut name = String::new();
        let mut offset = 0usize;
        let limit = 343usize;

        loop {
            let variables = json!({
                "uri": playlist_uri,
                "offset": offset,
                "limit": limit,
                "enableWatchFeedEntrypoint": false,
            });
            let response = self.graph_query("fetchPlaylist", variables).await?;
            if name.is_empty() {
                name = find_playlist_name(&response).unwrap_or_else(|| playlist_uri.to_string());
            }

            let before = items.len();
            collect_playlist_items(&response, &mut items);
            dedupe_items(&mut items);
            let loaded = items.len().saturating_sub(before);
            if loaded == 0 || loaded < limit {
                break;
            }
            offset += loaded;
        }

        Ok(PlaylistSnapshot {
            uri: playlist_uri.to_string(),
            name,
            items,
        })
    }

    pub async fn create_playlist(&self, name: &str) -> Result<SpotifyPlaylistSummary> {
        let headers = self.auth_headers().await?;
        let body = json!({
            "ops": [{
                "kind": 6,
                "updateListAttributes": {
                    "newAttributes": {
                        "values": {
                            "name": name,
                            "formatAttributes": [],
                            "pictureSize": []
                        },
                        "noValue": []
                    }
                }
            }]
        });
        let response = self
            .http()
            .post("https://spclient.wg.spotify.com/playlist/v2/playlist")
            .headers(headers.clone())
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(AppError::HttpStatus {
                url: "https://spclient.wg.spotify.com/playlist/v2/playlist".to_string(),
                status,
                body: body_excerpt(&text),
            });
        }
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        let uri = find_uri_with_prefix(&json, "spotify:playlist:").ok_or_else(|| {
            AppError::Spotify(
                "playlist create response did not contain spotify:playlist URI".to_string(),
            )
        })?;

        self.insert_playlist_into_rootlist(&uri, &headers).await?;
        Ok(SpotifyPlaylistSummary {
            uri,
            name: name.to_string(),
        })
    }

    pub async fn remove_playlist_items(&self, playlist_uri: &str, uids: &[String]) -> Result<()> {
        for chunk in uids.chunks(100) {
            let variables = json!({
                "playlistUri": playlist_uri,
                "uids": chunk,
            });
            self.graph_query("removeFromPlaylist", variables).await?;
        }
        Ok(())
    }

    pub async fn insert_playlist_items(
        &self,
        playlist_uri: &str,
        uris: &[String],
        after_uid: Option<&str>,
    ) -> Result<()> {
        let move_type = if after_uid.is_some() {
            "AFTER_UID"
        } else {
            "TOP_OF_PLAYLIST"
        };

        // Repeated insertion at one anchor reverses request order, so work
        // backward while preserving the order within each request.
        for chunk in uris.rchunks(ADD_BATCH_SIZE) {
            let variables = json!({
                "playlistUri": playlist_uri,
                "playlistItemUris": chunk,
                "newPosition": {
                    "moveType": move_type,
                    "fromUid": after_uid,
                }
            });
            self.graph_query("addToPlaylist", variables).await?;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        Ok(())
    }

    async fn insert_playlist_into_rootlist(
        &self,
        playlist_uri: &str,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<()> {
        let url = format!(
            "https://spclient.wg.spotify.com/playlist/v2/user/{}/rootlist/changes",
            self.username()
        );
        let body = json!({
            "deltas": [{
                "ops": [{
                    "kind": 2,
                    "add": {
                        "items": [{
                            "uri": playlist_uri,
                            "attributes": {
                                "timestamp": unix_timestamp(),
                                "formatAttributes": [],
                                "availableSignals": []
                            }
                        }],
                        "addFirst": true
                    }
                }],
                "info": {"source": {"client": 5}}
            }],
            "wantResultingRevisions": false,
            "wantSyncResult": false,
            "nonces": []
        });
        let response = self
            .http()
            .post(&url)
            .headers(headers.clone())
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(AppError::HttpStatus {
                url,
                status,
                body: body_excerpt(&text),
            });
        }
        Ok(())
    }
}

fn collect_playlists(value: &Value, playlists: &mut Vec<SpotifyPlaylistSummary>) {
    match value {
        Value::Object(map) => {
            if let Some(uri) = map.get("uri").and_then(Value::as_str)
                && uri.starts_with("spotify:playlist:")
                && let Some(name) = map.get("name").and_then(Value::as_str).or_else(|| {
                    map.get("profile")
                        .and_then(|profile| profile.get("name"))
                        .and_then(Value::as_str)
                })
            {
                playlists.push(SpotifyPlaylistSummary {
                    uri: uri.to_string(),
                    name: name.to_string(),
                });
            }
            for child in map.values() {
                collect_playlists(child, playlists);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_playlists(child, playlists);
            }
        }
        _ => {}
    }
}

fn collect_playlist_items(value: &Value, items: &mut Vec<SpotifyPlaylistItem>) {
    match value {
        Value::Object(map) => {
            if let Some(item) = parse_playlist_item(map) {
                items.push(item);
            }
            for child in map.values() {
                collect_playlist_items(child, items);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_playlist_items(child, items);
            }
        }
        _ => {}
    }
}

fn parse_playlist_item(map: &Map<String, Value>) -> Option<SpotifyPlaylistItem> {
    let uid = map.get("uid").and_then(Value::as_str)?;
    let mut tracks = Vec::new();
    search::collect_spotify_tracks(&Value::Object(map.clone()), &mut tracks);
    let track = tracks.into_iter().next()?;
    Some(SpotifyPlaylistItem {
        uid: uid.to_string(),
        track,
    })
}

fn dedupe_items(items: &mut Vec<SpotifyPlaylistItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.uid.clone()));
}

fn find_playlist_name(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(uri) = map.get("uri").and_then(Value::as_str)
                && uri.starts_with("spotify:playlist:")
                && let Some(name) = map.get("name").and_then(Value::as_str)
            {
                return Some(name.to_string());
            }
            map.values().find_map(find_playlist_name)
        }
        Value::Array(items) => items.iter().find_map(find_playlist_name),
        _ => None,
    }
}

fn find_uri_with_prefix(value: &Value, prefix: &str) -> Option<String> {
    match value {
        Value::String(text) => find_prefixed_uri(text, prefix),
        Value::Object(map) => map
            .values()
            .find_map(|child| find_uri_with_prefix(child, prefix)),
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_uri_with_prefix(child, prefix)),
        _ => None,
    }
}

fn find_prefixed_uri(text: &str, prefix: &str) -> Option<String> {
    let start = text.find(prefix)?;
    let uri = text[start..]
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '_' | '-'))
        .collect::<String>();
    (uri.len() > prefix.len()).then_some(uri)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_playlist_item_with_uid() {
        let value = json!({
            "uid": "item-1",
            "itemV2": {"data": {
                "uri": "spotify:track:abc",
                "name": "Song",
                "artists": {"items": [{"profile": {"name": "Artist"}}]}
            }}
        });
        let mut items = Vec::new();
        collect_playlist_items(&value, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].uid, "item-1");
        assert_eq!(items[0].track.uri, "spotify:track:abc");
    }

    #[test]
    fn extracts_playlist_uri_embedded_in_raw_response() {
        let value = Value::String(r#"ok spotify:playlist:abc123XYZ more"#.to_string());
        assert_eq!(
            find_uri_with_prefix(&value, "spotify:playlist:").as_deref(),
            Some("spotify:playlist:abc123XYZ")
        );
    }
}
