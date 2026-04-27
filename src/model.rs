#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoutubePlaylist {
    pub id: String,
    pub title: String,
    pub tracks: Vec<YoutubeTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YoutubeTrack {
    pub index: usize,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub video_id: String,
    pub thumbnails: Vec<String>,
}

impl YoutubeTrack {
    pub fn artist_display(&self) -> String {
        display_artists(&self.artists)
    }

    pub fn search_seed(&self) -> String {
        [
            self.artists.first().map(String::as_str),
            Some(&self.title),
            self.album.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyPlaylistSummary {
    pub uri: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSnapshot {
    pub uri: String,
    pub name: String,
    pub items: Vec<SpotifyPlaylistItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyPlaylistItem {
    pub uid: String,
    pub track: SpotifyTrack,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyTrack {
    pub uri: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub image_url: Option<String>,
}

impl SpotifyTrack {
    pub fn artist_display(&self) -> String {
        display_artists(&self.artists)
    }
}

fn display_artists(artists: &[String]) -> String {
    match artists {
        [] => "Unknown Artist".to_string(),
        artists => artists.join(", "),
    }
}

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub track: SpotifyTrack,
    pub score: f64,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct MatchedTrack {
    pub youtube: YoutubeTrack,
    pub spotify: SpotifyTrack,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct SkippedTrack {
    pub youtube: YoutubeTrack,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct OpencodeResolvedTrack {
    pub youtube: YoutubeTrack,
    pub spotify: SpotifyTrack,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MatchResult {
    pub matched: Vec<MatchedTrack>,
    pub skipped: Vec<SkippedTrack>,
    pub opencode_resolved: Vec<OpencodeResolvedTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub playlist_uri: String,
    pub current_uris: Vec<String>,
    pub desired_uris: Vec<String>,
    pub remove_uids: Vec<String>,
    pub add_uris: Vec<String>,
}

impl SyncPlan {
    pub fn is_noop(&self) -> bool {
        self.current_uris == self.desired_uris
    }
}
