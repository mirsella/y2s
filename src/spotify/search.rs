use std::collections::HashSet;
use std::time::Duration;

use reqwest::{StatusCode, header};
use serde_json::{Map, Value, json};

use crate::{
    error::{AppError, Result, body_excerpt},
    model::SpotifyTrack,
    spotify::SpotifyClient,
};

const SEARCH_ENDPOINT: &str = "https://api.spotify.com/v1/search";

impl SpotifyClient {
    pub async fn search_tracks(&self, query: &str, limit: usize) -> Result<Vec<SpotifyTrack>> {
        let mut tracks = Vec::new();
        let mut errors = Vec::new();

        for operation in ["searchSuggestions", "assistedCurationSearch"] {
            if !self.has_graph_hash(operation) {
                continue;
            }
            match self.search_tracks_graphql(operation, query, limit).await {
                Ok(mut found) => tracks.append(&mut found),
                Err(err) => errors.push(format!("{operation}: {err}")),
            }
        }

        if !tracks.is_empty() {
            return dedupe_and_limit(tracks, limit);
        }

        match self.search_tracks_web_api(query, limit).await {
            Ok(tracks) if !tracks.is_empty() => return Ok(tracks),
            Ok(_) => {}
            Err(err) => errors.push(format!("Web API fallback: {err}")),
        }

        search_error(errors)
    }

    async fn search_tracks_graphql(
        &self,
        operation: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SpotifyTrack>> {
        let limit = limit.clamp(1, 50) as u64;
        let variables = match operation {
            "searchSuggestions" => json!({
                "query": query,
                "offset": 0,
                "limit": limit,
                "numberOfTopResults": limit,
                "includeAuthors": true,
                "includeEpisodeContentRatingsV2": true,
            }),
            "assistedCurationSearch" => json!({
                "term": query,
                "limit": limit,
                "numberOfTopResults": limit,
            }),
            other => {
                return Err(AppError::Spotify(format!(
                    "unsupported Spotify search operation: {other}"
                )));
            }
        };

        let response = self.graph_query(operation, variables).await?;
        parse_search_tracks(&response, limit as usize)
    }

    async fn search_tracks_web_api(&self, query: &str, limit: usize) -> Result<Vec<SpotifyTrack>> {
        let response = self.search_tracks_web_api_response(query, limit).await?;
        parse_search_tracks(&response, limit)
    }

    async fn search_tracks_web_api_response(&self, query: &str, limit: usize) -> Result<Value> {
        let mut refreshed_access = false;
        let mut retried_after_rate_limit = false;
        let limit = limit.clamp(1, 50).to_string();

        loop {
            let headers = self.auth_headers().await?;
            let response = self
                .http()
                .get(SEARCH_ENDPOINT)
                .headers(headers)
                .query(&[
                    ("q", query),
                    ("type", "track"),
                    ("limit", limit.as_str()),
                    ("market", "from_token"),
                ])
                .send()
                .await?;
            let status = response.status();
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let text = response.text().await?;

            if status.is_success() {
                return serde_json::from_str(&text).map_err(|err| {
                    AppError::Spotify(format!(
                        "failed to parse Spotify search response as JSON: {err}; body: {}",
                        body_excerpt(&text)
                    ))
                });
            }

            if status == StatusCode::UNAUTHORIZED && !refreshed_access {
                self.refresh_access_token().await?;
                refreshed_access = true;
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS && !retried_after_rate_limit {
                let sleep_for = Duration::from_secs(retry_after.unwrap_or(3).min(30));
                tokio::time::sleep(sleep_for).await;
                retried_after_rate_limit = true;
                continue;
            }

            return Err(AppError::HttpStatus {
                url: SEARCH_ENDPOINT.to_string(),
                status,
                body: body_excerpt(&text),
            });
        }
    }
}

fn parse_search_tracks(response: &Value, limit: usize) -> Result<Vec<SpotifyTrack>> {
    let mut tracks = Vec::new();
    collect_spotify_tracks(response, &mut tracks);
    dedupe_and_limit(tracks, limit)
}

fn search_error(errors: Vec<String>) -> Result<Vec<SpotifyTrack>> {
    if errors.is_empty() {
        Ok(Vec::new())
    } else {
        Err(AppError::Spotify(format!(
            "Spotify search failed: {}",
            errors.join("; ")
        )))
    }
}

fn dedupe_and_limit(tracks: Vec<SpotifyTrack>, limit: usize) -> Result<Vec<SpotifyTrack>> {
    dedupe_tracks(tracks).map(|mut tracks| {
        tracks.truncate(limit);
        tracks
    })
}

pub(crate) fn collect_spotify_tracks(value: &Value, tracks: &mut Vec<SpotifyTrack>) {
    match value {
        Value::Object(map) => {
            if let Some(track) = parse_track_object(map) {
                tracks.push(track);
            }
            for child in map.values() {
                collect_spotify_tracks(child, tracks);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_spotify_tracks(child, tracks);
            }
        }
        _ => {}
    }
}

pub(crate) fn parse_track_object(map: &Map<String, Value>) -> Option<SpotifyTrack> {
    let uri = map.get("uri").and_then(Value::as_str)?;
    if !uri.starts_with("spotify:track:") {
        return None;
    }

    let title = map
        .get("name")
        .or_else(|| map.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string)?;
    let artists = parse_artists(map);
    let album = parse_album(map);
    let duration_ms = parse_duration(map);
    let image_url = parse_image_url(map);

    Some(SpotifyTrack {
        uri: uri.to_string(),
        title,
        artists,
        album,
        duration_ms,
        image_url,
    })
}

fn parse_artists(map: &Map<String, Value>) -> Vec<String> {
    let artists = map
        .get("artists")
        .and_then(|artists| artists.get("items"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(artist_name)
        .collect::<Vec<_>>();
    if !artists.is_empty() {
        return artists;
    }

    map.get("artists")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(artist_name)
        .collect()
}

fn artist_name(value: &Value) -> Option<String> {
    value
        .pointer("/profile/name")
        .and_then(Value::as_str)
        .or_else(|| value.get("name").and_then(Value::as_str))
        .map(str::to_string)
}

fn parse_album(map: &Map<String, Value>) -> Option<String> {
    map.get("albumOfTrack")
        .and_then(|album| album.get("name"))
        .and_then(Value::as_str)
        .or_else(|| {
            map.get("album")
                .and_then(|album| album.get("name"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
}

fn parse_duration(map: &Map<String, Value>) -> Option<u64> {
    map.get("duration")
        .and_then(|duration| duration.get("totalMilliseconds"))
        .and_then(Value::as_u64)
        .or_else(|| map.get("duration_ms").and_then(Value::as_u64))
        .or_else(|| map.get("durationMs").and_then(Value::as_u64))
}

fn parse_image_url(map: &Map<String, Value>) -> Option<String> {
    let source_arrays = [
        map.get("albumOfTrack")
            .and_then(|album| album.get("coverArt"))
            .and_then(|cover| cover.get("sources"))
            .and_then(Value::as_array),
        map.get("album")
            .and_then(|album| album.get("coverArt"))
            .and_then(|cover| cover.get("sources"))
            .and_then(Value::as_array),
        map.get("coverArt")
            .and_then(|cover| cover.get("sources"))
            .and_then(Value::as_array),
        map.get("album")
            .and_then(|album| album.get("images"))
            .and_then(Value::as_array),
        map.get("images").and_then(Value::as_array),
    ];

    source_arrays
        .into_iter()
        .flatten()
        .find_map(|sources| {
            sources
                .iter()
                .find_map(|source| source.get("url").and_then(Value::as_str))
        })
        .map(str::to_string)
}

fn dedupe_tracks(tracks: Vec<SpotifyTrack>) -> Result<Vec<SpotifyTrack>> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for track in tracks {
        if !track.uri.starts_with("spotify:track:") {
            return Err(AppError::Spotify(format!(
                "search parser produced a non-track URI: {}",
                track.uri
            )));
        }
        if seen.insert(track.uri.clone()) {
            deduped.push(track);
        }
    }
    Ok(deduped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_search_track_shape() {
        let value = json!({
            "data": {"searchV2": {"tracksV2": {"items": [{"item": {"data": {
                "uri": "spotify:track:abc",
                "name": "Song",
                "artists": {"items": [{"profile": {"name": "Artist"}}]},
                "albumOfTrack": {"name": "Album"},
                "duration": {"totalMilliseconds": 123000}
            }}}]}}}
        });
        let mut tracks = Vec::new();
        collect_spotify_tracks(&value, &mut tracks);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].artists, vec!["Artist"]);
    }

    #[test]
    fn parses_web_api_search_track_shape() {
        let value = json!({
            "tracks": {"items": [{
                "uri": "spotify:track:def",
                "name": "Song",
                "artists": [{"name": "Artist"}],
                "album": {"name": "Album", "images": [{"url": "https://image"}]},
                "duration_ms": 123000
            }]}
        });
        let mut tracks = Vec::new();
        collect_spotify_tracks(&value, &mut tracks);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].album.as_deref(), Some("Album"));
        assert_eq!(tracks[0].image_url.as_deref(), Some("https://image"));
    }
}
