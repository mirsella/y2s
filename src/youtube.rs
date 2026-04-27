use std::collections::HashSet;

use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, HeaderMap, HeaderValue, ORIGIN, REFERER, USER_AGENT,
};
use serde_json::{Value, json};
use url::Url;

use crate::{
    error::{AppError, Result, body_excerpt},
    model::{YoutubePlaylist, YoutubeTrack},
    progress::Progress,
};

const MUSIC_BROWSE: &str = "https://music.youtube.com/youtubei/v1/browse?prettyPrint=false";
const YOUTUBE_MUSIC_ORIGIN: &str = "https://music.youtube.com";
const YOUTUBE_CLIENT_VERSION: &str = "1.20240424.01.00";

static RAW_PLAYLIST_ID: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_-]{10,}$").expect("playlist id regex is valid"));

pub async fn fetch_playlist(
    input: &str,
    limit: Option<usize>,
    progress: &Progress,
) -> Result<YoutubePlaylist> {
    let playlist_id = parse_playlist_id(input)?;
    let client = reqwest::Client::builder()
        .default_headers(default_headers()?)
        .build()?;

    let spinner = progress.spinner(format!("YouTube playlist {playlist_id}"));

    let mut tracks = Vec::new();
    let mut seen_video_ids = HashSet::new();
    let mut title = None;
    let mut continuation = None;
    let mut page = 0usize;

    loop {
        let body = match continuation.take() {
            Some(token) => json!({
                "context": innertube_context(),
                "continuation": token,
            }),
            None => json!({
                "context": innertube_context(),
                "browseId": browse_id(&playlist_id),
            }),
        };

        let response = client.post(MUSIC_BROWSE).json(&body).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(AppError::HttpStatus {
                url: MUSIC_BROWSE.to_string(),
                status,
                body: body_excerpt(&text),
            });
        }

        let json: Value = serde_json::from_str(&text).map_err(|err| {
            AppError::Youtube(format!(
                "YouTube returned invalid JSON for playlist {playlist_id}: {err}"
            ))
        })?;

        if title.is_none() {
            title = find_playlist_title(&json);
        }

        let before = tracks.len();
        collect_tracks(&json, &mut tracks, &mut seen_video_ids, limit);
        let loaded = tracks.len().saturating_sub(before);
        spinner.set_message(format!(
            "YouTube playlist {playlist_id}: page {} loaded {} tracks ({} total)",
            page + 1,
            loaded,
            tracks.len()
        ));

        if limit.is_some_and(|max| tracks.len() >= max) {
            tracks.truncate(limit.expect("checked Some"));
            break;
        }

        continuation = find_playlist_continuation(&json);
        page += 1;

        if continuation.is_none() {
            break;
        }
    }

    spinner.finish_and_clear();

    if tracks.is_empty() {
        return Err(AppError::Youtube(format!(
            "no tracks found for playlist {playlist_id}; it may be private, unavailable, or not a compatible public YouTube playlist"
        )));
    }

    Ok(YoutubePlaylist {
        id: playlist_id.clone(),
        title: title.unwrap_or(playlist_id),
        tracks,
    })
}

pub fn parse_playlist_id(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(AppError::InvalidInput(
            "YouTube playlist URL or ID cannot be empty".to_string(),
        ));
    }

    if let Ok(url) = Url::parse(trimmed) {
        if let Some((_, value)) = url.query_pairs().find(|(key, _)| key == "list") {
            let value = value.trim().to_string();
            if value.is_empty() {
                return Err(AppError::InvalidInput(
                    "YouTube URL has an empty list= parameter".to_string(),
                ));
            }
            return Ok(value);
        }

        return Err(AppError::InvalidInput(format!(
            "YouTube URL does not contain a list= playlist parameter: {trimmed}"
        )));
    }

    if RAW_PLAYLIST_ID.is_match(trimmed) {
        Ok(trimmed.to_string())
    } else {
        Err(AppError::InvalidInput(format!(
            "not a valid YouTube playlist URL or raw playlist ID: {trimmed}"
        )))
    }
}

fn default_headers() -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123 Safari/537.36",
        ),
    );
    headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert(ORIGIN, HeaderValue::from_static(YOUTUBE_MUSIC_ORIGIN));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://music.youtube.com/"),
    );
    Ok(headers)
}

fn innertube_context() -> Value {
    json!({
        "client": {
            "clientName": "WEB_REMIX",
            "clientVersion": YOUTUBE_CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        }
    })
}

fn browse_id(playlist_id: &str) -> String {
    if playlist_id.starts_with("VL") {
        playlist_id.to_string()
    } else {
        format!("VL{playlist_id}")
    }
}

fn find_playlist_title(value: &Value) -> Option<String> {
    let title_paths = [
        "/header/musicDetailHeaderRenderer/title/runs/0/text",
        "/header/musicEditablePlaylistDetailHeaderRenderer/header/musicDetailHeaderRenderer/title/runs/0/text",
        "/metadata/playlistMetadataRenderer/title",
        "/microformat/microformatDataRenderer/title",
    ];

    for path in title_paths {
        if let Some(title) = value.pointer(path).and_then(Value::as_str) {
            let title = clean_text(title);
            if !title.is_empty() {
                return Some(title);
            }
        }
    }

    find_object_with_key(value, "musicDetailHeaderRenderer")
        .and_then(|header| header.pointer("/title/runs/0/text"))
        .and_then(Value::as_str)
        .map(clean_text)
        .filter(|title| !title.is_empty())
}

fn collect_tracks(
    value: &Value,
    tracks: &mut Vec<YoutubeTrack>,
    seen_video_ids: &mut HashSet<String>,
    limit: Option<usize>,
) {
    if limit.is_some_and(|max| tracks.len() >= max) {
        return;
    }

    match value {
        Value::Object(map) => {
            if let Some(renderer) = map.get("musicResponsiveListItemRenderer")
                && let Some(track) = parse_track(renderer, tracks.len())
                && seen_video_ids.insert(track.video_id.clone())
            {
                tracks.push(track);
            }

            for child in map.values() {
                collect_tracks(child, tracks, seen_video_ids, limit);
                if limit.is_some_and(|max| tracks.len() >= max) {
                    return;
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_tracks(child, tracks, seen_video_ids, limit);
                if limit.is_some_and(|max| tracks.len() >= max) {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn parse_track(renderer: &Value, index: usize) -> Option<YoutubeTrack> {
    let video_id = renderer
        .pointer("/playlistItemData/videoId")
        .and_then(Value::as_str)
        .or_else(|| find_string_by_key(renderer, "videoId"))?
        .to_string();

    let title = renderer
        .pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
        .and_then(Value::as_str)
        .map(clean_text)
        .filter(|title| !title.is_empty())?;

    let mut artists = Vec::new();
    let mut album = None;

    if let Some(runs) = renderer
        .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
        .and_then(Value::as_array)
    {
        for run in runs {
            let Some(text) = run.get("text").and_then(Value::as_str).map(clean_text) else {
                continue;
            };
            if text.is_empty() || is_separator(&text) || looks_like_duration(&text) {
                continue;
            }

            let page_type = run
                .pointer("/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
                .and_then(Value::as_str);

            match page_type {
                Some("MUSIC_PAGE_TYPE_ARTIST") | Some("MUSIC_PAGE_TYPE_USER_CHANNEL") => {
                    artists.push(text)
                }
                Some("MUSIC_PAGE_TYPE_ALBUM") => album = Some(text),
                _ if artists.is_empty() => artists.push(text),
                _ => {}
            }
        }
    }

    if artists.is_empty() {
        artists = renderer
            .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|run| run.get("text").and_then(Value::as_str).map(clean_text))
            .filter(|text| !text.is_empty() && !is_separator(text) && !looks_like_duration(text))
            .take(1)
            .collect();
    }

    let duration_ms = find_duration(renderer).map(|seconds| seconds * 1000);
    let thumbnails = collect_thumbnail_urls(renderer);

    Some(YoutubeTrack {
        index,
        title,
        artists,
        album,
        duration_ms,
        video_id,
        thumbnails,
    })
}

fn find_duration(renderer: &Value) -> Option<u64> {
    let fixed = renderer
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text")
        .and_then(Value::as_str)
        .and_then(parse_duration);
    if fixed.is_some() {
        return fixed;
    }

    let mut durations = Vec::new();
    collect_strings(renderer, &mut |text| {
        if looks_like_duration(text)
            && let Some(duration) = parse_duration(text)
        {
            durations.push(duration);
        }
    });
    durations.into_iter().next()
}

fn parse_duration(text: &str) -> Option<u64> {
    let parts = text
        .trim()
        .split(':')
        .map(str::parse::<u64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    match parts.as_slice() {
        [minutes, seconds] if *seconds < 60 => Some(minutes * 60 + seconds),
        [hours, minutes, seconds] if *minutes < 60 && *seconds < 60 => {
            Some(hours * 3600 + minutes * 60 + seconds)
        }
        _ => None,
    }
}

fn looks_like_duration(text: &str) -> bool {
    static DURATION: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^\d{1,2}:\d{2}(:\d{2})?$").expect("duration regex"));
    DURATION.is_match(text.trim())
}

fn collect_thumbnail_urls(renderer: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    collect_objects(renderer, &mut |object| {
        if let Some(url) = object.get("url").and_then(Value::as_str)
            && url.starts_with("http")
            && !urls.iter().any(|existing| existing == url)
        {
            urls.push(url.to_string());
        }
    });
    urls
}

fn find_playlist_continuation(value: &Value) -> Option<String> {
    if let Some(token) = find_track_continuation(value) {
        return Some(token);
    }

    let mut found = None;
    collect_objects(value, &mut |object| {
        if found.is_some() {
            return;
        }

        let in_playlist_shelf = object.contains_key("musicPlaylistShelfContinuation")
            || object.contains_key("musicPlaylistShelfRenderer")
            || object.contains_key("continuationContents");
        if !in_playlist_shelf && !object.contains_key("nextContinuationData") {
            return;
        }

        if let Some(token) = object
            .get("nextContinuationData")
            .and_then(|next| next.get("continuation"))
            .and_then(Value::as_str)
            .or_else(|| {
                object
                    .get("reloadContinuationData")
                    .and_then(|next| next.get("continuation"))
                    .and_then(Value::as_str)
            })
        {
            found = Some(token.to_string());
        }
    });
    found
}

fn find_track_continuation(value: &Value) -> Option<String> {
    let mut found = None;
    collect_objects(value, &mut |object| {
        if found.is_some() {
            return;
        }

        if let Some(token) = object
            .get("continuationItemRenderer")
            .and_then(|renderer| renderer.get("continuationEndpoint"))
            .and_then(|endpoint| endpoint.get("continuationCommand"))
            .and_then(|command| command.get("token"))
            .and_then(Value::as_str)
        {
            found = Some(token.to_string());
        }
    });
    found
}

fn find_object_with_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found);
            }
            map.values()
                .find_map(|child| find_object_with_key(child, key))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_object_with_key(child, key)),
        _ => None,
    }
}

fn find_string_by_key<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(Value::as_str) {
                return Some(found);
            }
            map.values()
                .find_map(|child| find_string_by_key(child, key))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_by_key(child, key)),
        _ => None,
    }
}

fn collect_objects<'a>(
    value: &'a Value,
    visitor: &mut impl FnMut(&'a serde_json::Map<String, Value>),
) {
    match value {
        Value::Object(map) => {
            visitor(map);
            for child in map.values() {
                collect_objects(child, visitor);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_objects(child, visitor);
            }
        }
        _ => {}
    }
}

fn collect_strings<'a>(value: &'a Value, visitor: &mut impl FnMut(&'a str)) {
    match value {
        Value::String(text) => visitor(text),
        Value::Object(map) => {
            for child in map.values() {
                collect_strings(child, visitor);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_strings(child, visitor);
            }
        }
        _ => {}
    }
}

fn clean_text(text: &str) -> String {
    text.replace('\u{00a0}', " ").trim().to_string()
}

fn is_separator(text: &str) -> bool {
    matches!(text.trim(), "•" | "·" | "-" | "–" | "—" | "|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_playlist_url() {
        let id = parse_playlist_id(
            "https://www.youtube.com/watch?v=abc&list=PLnYA0n5BTNscRlnFBkNGJrCyKdOqGtID9&index=1",
        )
        .unwrap();
        assert_eq!(id, "PLnYA0n5BTNscRlnFBkNGJrCyKdOqGtID9");
    }

    #[test]
    fn parses_raw_playlist_id() {
        assert_eq!(parse_playlist_id("PL1234567890").unwrap(), "PL1234567890");
    }

    #[test]
    fn rejects_url_without_list() {
        let err = parse_playlist_id("https://www.youtube.com/watch?v=abc").unwrap_err();
        assert!(err.to_string().contains("list="));
    }

    #[test]
    fn parses_duration() {
        assert_eq!(super::parse_duration("3:42"), Some(222));
        assert_eq!(super::parse_duration("1:02:03"), Some(3723));
    }

    #[test]
    fn prefers_playlist_item_continuation_over_page_continuation() {
        let value = json!({
            "contents": {
                "musicPlaylistShelfRenderer": {
                    "contents": [{
                        "continuationItemRenderer": {
                            "continuationEndpoint": {
                                "continuationCommand": {
                                    "token": "track-page-token",
                                    "request": "CONTINUATION_REQUEST_TYPE_BROWSE"
                                }
                            }
                        }
                    }]
                }
            },
            "continuations": [{
                "nextContinuationData": {
                    "continuation": "related-playlists-token"
                }
            }]
        });

        assert_eq!(
            find_playlist_continuation(&value).as_deref(),
            Some("track-page-token")
        );
    }
}
