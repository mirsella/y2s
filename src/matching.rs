use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use dialoguer::{Input, Select, theme::ColorfulTheme};
use indicatif::ProgressBar;
use once_cell::sync::Lazy;
use regex::Regex;
use strsim::jaro_winkler;
use tokio::task::JoinSet;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{
    error::{AppError, Result},
    model::{
        MatchResult, MatchedTrack, OpencodeResolvedTrack, PlaylistSnapshot, ScoredCandidate,
        SkippedTrack, SpotifyTrack, YoutubeTrack,
    },
    opencode::OpencodeResolver,
    progress::Progress,
    spotify::SpotifyClient,
};

const AUTO_ACCEPT_SCORE: f64 = 84.0;
const AUTO_ACCEPT_GAP: f64 = 7.5;
const DURATION_TIE_SCORE: f64 = 84.0;
const CLOSE_DURATION_TEXT_ARTIST_SCORE: f64 = 70.0;
const CLOSE_DURATION_TEXT_ARTIST_SECONDS: f64 = 1.0;
const DURATION_AUTO_TOLERANCE_SECONDS: f64 = 2.0;
const EQUAL_SCORE_EPSILON: f64 = 0.5;
const TITLE_AUTO_THRESHOLD: f64 = 0.86;
const PRIMARY_ARTIST_AUTO_THRESHOLD: f64 = 0.82;
const ARTIST_SET_AUTO_THRESHOLD: f64 = 0.86;
const DURATION_COMPATIBLE_SECONDS: f64 = 8.0;
const EXISTING_INDEX_TITLE_THRESHOLD: f64 = 0.70;
const EXISTING_INDEX_ARTIST_THRESHOLD: f64 = 0.65;
const EXISTING_INDEX_DURATION_SECONDS: f64 = 35.0;
const EXISTING_PLAYLIST_TITLE_THRESHOLD: f64 = 0.70;
const EXISTING_PLAYLIST_ARTIST_THRESHOLD: f64 = 0.65;
const EXISTING_PLAYLIST_ARTIST_SET_THRESHOLD: f64 = 0.74;
const EXISTING_PLAYLIST_DURATION_SECONDS: f64 = 45.0;
const EXISTING_PLAYLIST_MIN_SCORE: f64 = 58.0;
const EXISTING_SEQUENCE_TITLE_THRESHOLD: f64 = 0.62;
const EXISTING_SEQUENCE_MIN_SCORE: f64 = 45.0;
const EXISTING_SEQUENCE_DURATION_SECONDS: f64 = 180.0;
const EXISTING_PLAYLIST_LOOKAHEAD: usize = 8;
const SPOTIFY_SEARCH_LIMIT: usize = 15;
const MAX_SEARCH_QUERIES: usize = 8;
const OPENCODE_CANDIDATE_LIMIT: usize = 12;
const LOCAL_REUSE_CANDIDATE_LIMIT: usize = 8;

enum CandidateDecision {
    Accept {
        candidate: ScoredCandidate,
        opencode_reason: Option<String>,
    },
    Prompt {
        candidates: Vec<ScoredCandidate>,
        opencode_rejection: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct LocalReuseCandidate {
    existing_index: usize,
    candidate: ScoredCandidate,
}

impl CandidateDecision {
    fn is_prompt(&self) -> bool {
        matches!(self, Self::Prompt { .. })
    }
}

pub async fn resolve_playlist(
    spotify: &SpotifyClient,
    youtube_tracks: &[YoutubeTrack],
    current: &PlaylistSnapshot,
    concurrency: usize,
    opencode: Option<&OpencodeResolver>,
    progress: &Progress,
) -> Result<MatchResult> {
    let mut result = MatchResult::default();
    let mut cache: HashMap<String, Option<ScoredCandidate>> = HashMap::new();
    let mut used_existing = HashSet::new();
    let existing_matches =
        reuse_existing_matches(youtube_tracks, current, &mut used_existing, progress).await;

    for (youtube, candidate) in youtube_tracks.iter().cloned().zip(existing_matches.iter()) {
        if let Some(candidate) = candidate {
            cache.insert(decision_cache_key(&youtube), Some(candidate.clone()));
            push_match(&mut result, youtube, candidate.clone());
        }
    }

    let search_inputs = youtube_tracks
        .iter()
        .cloned()
        .enumerate()
        .filter(|(index, _)| existing_matches[*index].is_none())
        .map(|(_, track)| track)
        .collect::<Vec<_>>();

    if search_inputs.is_empty() {
        result.matched.sort_by_key(|matched| matched.youtube.index);
        return Ok(result);
    }

    let search_count = search_inputs.len();
    let bar = progress.track_bar(search_count, "searching Spotify");
    let mut inputs = search_inputs.into_iter();
    let mut tasks = JoinSet::new();
    for _ in 0..concurrency.max(1).min(search_count) {
        if let Some(youtube) = inputs.next() {
            spawn_search_task(
                &mut tasks,
                spotify.clone(),
                opencode.cloned(),
                progress.clone(),
                youtube,
            );
        }
    }

    let mut completed = 0usize;
    let mut ambiguous = 0usize;
    while let Some(joined) = tasks.join_next().await {
        if let Some(youtube) = inputs.next() {
            spawn_search_task(
                &mut tasks,
                spotify.clone(),
                opencode.cloned(),
                progress.clone(),
                youtube,
            );
        }

        let (youtube, decision_result) = match joined {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("ERROR: Spotify search task failed: {err}");
                completed += 1;
                sync_search_progress(&bar, progress, completed, ambiguous);
                continue;
            }
        };

        completed += 1;
        let needs_manual_review = opencode.is_none()
            && decision_result
                .as_ref()
                .is_ok_and(CandidateDecision::is_prompt);
        if needs_manual_review {
            ambiguous += 1;
        }

        let cache_key = decision_cache_key(&youtube);
        if let Some(cached) = cache.get(&cache_key) {
            if needs_manual_review {
                ambiguous = ambiguous.saturating_sub(1);
            }
            apply_cached_decision(&mut result, youtube, cached.clone());
            sync_search_progress(&bar, progress, completed, ambiguous);
            continue;
        }

        let decision = match decision_result {
            Ok(decision) => decision,
            Err(err) => {
                push_skip(
                    &mut result,
                    youtube,
                    format!("match resolution failed: {err}"),
                );
                cache.insert(cache_key, None);
                sync_search_progress(&bar, progress, completed, ambiguous);
                continue;
            }
        };

        match decision {
            CandidateDecision::Accept {
                candidate,
                opencode_reason,
            } => {
                if opencode_reason.is_some() {
                    push_opencode_resolved(&mut result, &youtube, &candidate, opencode_reason);
                }
                cache.insert(cache_key, Some(candidate.clone()));
                push_match(&mut result, youtube, candidate);
            }
            CandidateDecision::Prompt {
                candidates,
                opencode_rejection,
            } => {
                if opencode.is_some() {
                    cache.insert(cache_key, None);
                    push_skip(
                        &mut result,
                        youtube,
                        opencode_skip_reason(opencode_rejection),
                    );
                    sync_search_progress(&bar, progress, completed, ambiguous);
                    continue;
                }

                let pause = progress.pause_rendering();
                let decision = prompt_manual_or_skip(spotify, &youtube, candidates).await?;
                drop(pause);
                ambiguous = ambiguous.saturating_sub(1);
                cache.insert(cache_key, decision.clone());
                match decision {
                    Some(candidate) => push_match(&mut result, youtube, candidate),
                    None => push_skip(
                        &mut result,
                        youtube,
                        "skipped by user or no acceptable match".to_string(),
                    ),
                }
            }
        }
        sync_search_progress(&bar, progress, completed, ambiguous);
    }
    bar.finish_and_clear();

    sort_match_result(&mut result);

    Ok(result)
}

fn spawn_search_task(
    tasks: &mut JoinSet<(YoutubeTrack, Result<CandidateDecision>)>,
    spotify: SpotifyClient,
    opencode: Option<OpencodeResolver>,
    progress: Progress,
    youtube: YoutubeTrack,
) {
    tasks.spawn(async move {
        let task = progress.spinner(format!(
            "searching #{} {} - {}",
            youtube.index + 1,
            youtube.artist_display(),
            youtube.title
        ));
        let decision = search_and_classify(&spotify, &youtube).await;
        if opencode.is_some() && decision.as_ref().is_ok_and(CandidateDecision::is_prompt) {
            set_task_message(&progress, &task, "asking opencode", &youtube);
        }
        let decision = resolve_with_opencode(opencode.as_ref(), &youtube, decision).await;
        set_task_message(
            &progress,
            &task,
            if decision.as_ref().is_ok_and(CandidateDecision::is_prompt) {
                if opencode.is_some() {
                    "skipped"
                } else {
                    "needs review"
                }
            } else {
                "matched"
            },
            &youtube,
        );
        task.finish_and_clear();
        (youtube, decision)
    });
}

fn sync_search_progress(
    bar: &ProgressBar,
    progress: &Progress,
    completed: usize,
    ambiguous: usize,
) {
    if progress.rendering_paused() {
        return;
    }
    bar.set_position(completed as u64);
    bar.set_message(if ambiguous == 0 {
        "searching Spotify".to_string()
    } else {
        format!("searching Spotify · {ambiguous} need review")
    });
}

fn sort_match_result(result: &mut MatchResult) {
    result.matched.sort_by_key(|matched| matched.youtube.index);
    result.skipped.sort_by_key(|skipped| skipped.youtube.index);
    result
        .opencode_resolved
        .sort_by_key(|resolved| resolved.youtube.index);
}

async fn resolve_with_opencode(
    opencode: Option<&OpencodeResolver>,
    youtube: &YoutubeTrack,
    decision: Result<CandidateDecision>,
) -> Result<CandidateDecision> {
    let (candidates, opencode_rejection) = match decision? {
        CandidateDecision::Accept {
            candidate,
            opencode_reason,
        } => {
            return Ok(CandidateDecision::Accept {
                candidate,
                opencode_reason,
            });
        }
        CandidateDecision::Prompt {
            candidates,
            opencode_rejection,
        } => (candidates, opencode_rejection),
    };

    let Some(opencode) = opencode else {
        return Ok(CandidateDecision::Prompt {
            candidates,
            opencode_rejection,
        });
    };

    match opencode.resolve(youtube, &candidates).await {
        Ok(resolution) => match resolution.candidate {
            Some(candidate) => Ok(CandidateDecision::Accept {
                candidate,
                opencode_reason: resolution.reason,
            }),
            None => Ok(CandidateDecision::Prompt {
                candidates,
                opencode_rejection: Some(resolution.rejection_reason()),
            }),
        },
        Err(err) => Ok(CandidateDecision::Prompt {
            candidates,
            opencode_rejection: Some(format!("opencode failed: {err}")),
        }),
    }
}

async fn search_and_classify(
    spotify: &SpotifyClient,
    youtube: &YoutubeTrack,
) -> Result<CandidateDecision> {
    let mut candidates = search_candidates(spotify, youtube).await?;
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    candidates.truncate(OPENCODE_CANDIDATE_LIMIT);
    Ok(classify_candidates(youtube, candidates))
}

async fn search_candidates(
    spotify: &SpotifyClient,
    youtube: &YoutubeTrack,
) -> Result<Vec<ScoredCandidate>> {
    let mut tracks = Vec::new();
    let mut errors = Vec::new();
    for query in search_queries(youtube) {
        match spotify.search_tracks(&query, SPOTIFY_SEARCH_LIMIT).await {
            Ok(mut results) => tracks.append(&mut results),
            Err(err) => errors.push(format!("{query}: {err}")),
        }
    }

    if tracks.is_empty() && !errors.is_empty() {
        return Err(AppError::Spotify(format!(
            "all Spotify search queries failed for {} - {}: {}",
            youtube.artist_display(),
            youtube.title,
            errors.join("; ")
        )));
    }

    let mut seen = HashMap::new();
    for track in tracks {
        seen.entry(track.uri.clone()).or_insert(track);
    }

    let mut candidates = seen
        .into_values()
        .map(|track| score_candidate(youtube, track))
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(candidates)
}

fn search_queries(youtube: &YoutubeTrack) -> Vec<String> {
    let mut queries = Vec::new();

    let artist_variants = search_artist_variants(youtube);
    let title_variants = search_title_variants(youtube);
    let album = youtube
        .album
        .as_deref()
        .map(search_clean_text)
        .filter(|album| !album.is_empty());

    for title in &title_variants {
        for artist in &artist_variants {
            queries.extend([
                compact_spaces(format!("{artist} {title}")),
                compact_spaces(format!("track:{title} artist:{artist}")),
            ]);
            if let Some(album) = &album {
                queries.push(compact_spaces(format!("{artist} {title} {album}")));
            }
        }
        queries.push(title.clone());
    }

    dedupe_preserve_order(&mut queries);
    queries.truncate(MAX_SEARCH_QUERIES);
    queries
}

fn search_artist_variants(youtube: &YoutubeTrack) -> Vec<String> {
    let mut variants = Vec::new();
    if let Some(primary) = youtube.artists.first() {
        variants.push(search_clean_text(primary));
    }
    if youtube.artists.len() > 1 {
        variants.push(search_clean_text(&youtube.artists.join(" ")));
        for artist in &youtube.artists {
            variants.push(search_clean_text(artist));
        }
    }
    dedupe_preserve_order(&mut variants);
    variants.retain(|variant| !variant.is_empty());
    variants
}

fn search_title_variants(youtube: &YoutubeTrack) -> Vec<String> {
    let mut variants = Vec::new();
    let title = youtube.title.as_str();
    let no_brackets = remove_bracketed_segments(&clean_search_title(title));

    for title in [
        title.to_string(),
        clean_search_title(title),
        remove_bracketed_segments(title),
        remove_presentation_labels(title),
        no_brackets.clone(),
        remove_presentation_labels(&no_brackets),
    ] {
        push_search_title_variant(&mut variants, &title, youtube);
    }

    dedupe_preserve_order(&mut variants);
    variants.retain(|variant| !variant.is_empty());
    variants
}

fn push_search_title_variant(variants: &mut Vec<String>, title: &str, youtube: &YoutubeTrack) {
    let stripped_prefix = strip_leading_artist_prefixes(title, &youtube.artists);
    variants.push(search_clean_text(&remove_presentation_labels(
        &stripped_prefix,
    )));
}

fn clean_search_title(title: &str) -> String {
    static CREDIT_SEGMENT: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)\s*((\(|\[)\s*with\b|(\(|\[|\b)(feat\.?|ft\.?|featuring|prod\.?)).*$")
            .expect("credit regex is valid")
    });

    let stripped = CREDIT_SEGMENT.replace_all(title, " ");
    compact_spaces(stripped.as_ref())
}

fn remove_bracketed_segments(title: &str) -> String {
    static BRACKETED: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"[\(\[\{][^\)\]\}]*[\)\]\}]").expect("bracket regex is valid"));

    compact_spaces(BRACKETED.replace_all(title, " ").as_ref())
}

fn remove_presentation_labels(title: &str) -> String {
    static PRESENTATION: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r#"(?ix)
            \b(
                official|music\s+video|video|lyrics?|lyric\s+video|audio|visualizer|
                visualette|clip|hd|4k
            )\b|
            \bfrom\s+["“”][^"“”]+["“”]
            "#,
        )
        .expect("presentation regex is valid")
    });

    compact_spaces(PRESENTATION.replace_all(title, " ").as_ref())
}

fn strip_leading_artist_prefixes(title: &str, artists: &[String]) -> String {
    static PREFIX: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"^\s*([^\-–—:|]+?)\s*[\-–—:|]\s*(.+)$").expect("prefix regex is valid")
    });

    let mut current = title.trim().to_string();
    while let Some(captures) = PREFIX.captures(&current) {
        let prefix = captures
            .get(1)
            .map(|capture| capture.as_str())
            .unwrap_or("");
        let rest = captures
            .get(2)
            .map(|capture| capture.as_str())
            .unwrap_or("");
        let matches_artist = artists
            .iter()
            .any(|artist| similarity(prefix, artist) >= 0.90);
        if !matches_artist {
            break;
        }
        current = rest.trim().to_string();
    }
    current
}

fn search_clean_text(input: &str) -> String {
    static SEARCH_PUNCT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"[^\p{L}\p{N}:]+").expect("search punct regex is valid"));

    let folded = ascii_fold(input).to_lowercase();
    compact_spaces(SEARCH_PUNCT.replace_all(&folded, " ").as_ref())
}

fn compact_spaces(input: impl AsRef<str>) -> String {
    static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("spaces regex is valid"));
    SPACES.replace_all(input.as_ref().trim(), " ").to_string()
}

fn dedupe_preserve_order(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

async fn reuse_existing_matches(
    youtube_tracks: &[YoutubeTrack],
    current: &PlaylistSnapshot,
    used_existing: &mut HashSet<usize>,
    progress: &Progress,
) -> Vec<Option<ScoredCandidate>> {
    let mut matches = vec![None; youtube_tracks.len()];
    if current.items.is_empty() {
        return matches;
    }

    if current.items.len() == youtube_tracks.len() {
        for (index, youtube) in youtube_tracks.iter().enumerate() {
            let Some(item) = current.items.get(index) else {
                continue;
            };
            let candidate = score_candidate(youtube, item.track.clone());
            if same_index_existing_match_is_safe(youtube, &candidate) {
                used_existing.insert(index);
                matches[index] = Some(candidate);
            }
        }
    }

    if matches.iter().all(Option::is_some) {
        return matches;
    }

    merge_existing_matches_in_order(youtube_tracks, current, &mut matches, used_existing);

    let remaining_indices = matches
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.is_none().then_some(index))
        .collect::<Vec<_>>();
    if remaining_indices.is_empty() {
        return matches;
    }

    let local_candidates =
        score_existing_playlist_candidates(youtube_tracks, current, &remaining_indices, progress)
            .await;

    for (youtube_index, candidates) in local_candidates {
        let youtube = &youtube_tracks[youtube_index];
        if matches[youtube_index].is_some() {
            continue;
        }

        let mut available = candidates
            .iter()
            .filter(|candidate| !used_existing.contains(&candidate.existing_index))
            .filter(|candidate| existing_playlist_match_is_safe(youtube, &candidate.candidate));

        let Some(top) = available.next() else {
            continue;
        };
        let gap = available
            .next()
            .map(|second| top.candidate.score - second.candidate.score)
            .unwrap_or(100.0);
        if gap >= AUTO_ACCEPT_GAP {
            used_existing.insert(top.existing_index);
            matches[youtube_index] = Some(top.candidate.clone());
        }
    }

    matches
}

fn merge_existing_matches_in_order(
    youtube_tracks: &[YoutubeTrack],
    current: &PlaylistSnapshot,
    matches: &mut [Option<ScoredCandidate>],
    used_existing: &mut HashSet<usize>,
) {
    let mut next_youtube_index = 0;

    for (existing_index, item) in current.items.iter().enumerate() {
        if used_existing.contains(&existing_index) {
            continue;
        }

        let candidate = (next_youtube_index..youtube_tracks.len())
            .take(EXISTING_PLAYLIST_LOOKAHEAD + 1)
            .filter(|youtube_index| matches[*youtube_index].is_none())
            .find_map(|youtube_index| {
                let youtube = &youtube_tracks[youtube_index];
                let candidate = score_candidate(youtube, item.track.clone());
                existing_sequence_match_is_safe(youtube, &candidate).then_some((
                    youtube_index,
                    LocalReuseCandidate {
                        existing_index,
                        candidate,
                    },
                ))
            });

        let Some((youtube_index, candidate)) = candidate else {
            continue;
        };

        used_existing.insert(existing_index);
        matches[youtube_index] = Some(candidate.candidate);
        next_youtube_index = youtube_index + 1;
    }
}

async fn score_existing_playlist_candidates(
    youtube_tracks: &[YoutubeTrack],
    current: &PlaylistSnapshot,
    youtube_indices: &[usize],
    progress: &Progress,
) -> Vec<(usize, Vec<LocalReuseCandidate>)> {
    let worker_count = thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
        .min(youtube_indices.len());
    let task_spinners = (0..worker_count)
        .map(|_| progress.spinner("waiting for playlist matching"))
        .collect::<Vec<_>>();
    let bar = progress.track_bar(
        youtube_indices.len(),
        "matching against current Spotify playlist",
    );

    let youtube_tracks = Arc::new(youtube_tracks.to_vec());
    let youtube_indices = Arc::new(youtube_indices.to_vec());
    let current_tracks = Arc::new(
        current
            .items
            .iter()
            .enumerate()
            .map(|(index, item)| (index, item.track.clone()))
            .collect::<Vec<_>>(),
    );
    let next_index = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::with_capacity(worker_count);

    for task in task_spinners {
        let youtube_tracks = Arc::clone(&youtube_tracks);
        let youtube_indices = Arc::clone(&youtube_indices);
        let current_tracks = Arc::clone(&current_tracks);
        let next_index = Arc::clone(&next_index);
        let bar = bar.clone();
        let progress = progress.clone();
        handles.push(tokio::task::spawn_blocking(move || {
            let mut worker_results = Vec::new();
            loop {
                let youtube_index = next_index.fetch_add(1, Ordering::Relaxed);
                if youtube_index >= youtube_indices.len() {
                    task.finish_and_clear();
                    return worker_results;
                }

                let youtube_index = youtube_indices[youtube_index];
                let youtube = &youtube_tracks[youtube_index];
                set_task_message(&progress, &task, "matching", youtube);
                let candidates = top_existing_candidates(youtube, &current_tracks);
                bar.inc(1);
                worker_results.push((youtube_index, candidates));
            }
        }));
    }

    let mut candidates = Vec::new();
    for handle in handles {
        if let Ok(worker_results) = handle.await {
            candidates.extend(worker_results);
        }
    }
    bar.finish_and_clear();
    candidates.sort_by_key(|(youtube_index, _)| *youtube_index);

    candidates
}

fn top_existing_candidates(
    youtube: &YoutubeTrack,
    current_tracks: &[(usize, SpotifyTrack)],
) -> Vec<LocalReuseCandidate> {
    let mut top = Vec::new();
    for (existing_index, track) in current_tracks {
        push_top_existing_candidate(
            &mut top,
            LocalReuseCandidate {
                existing_index: *existing_index,
                candidate: score_candidate(youtube, track.clone()),
            },
        );
    }
    top
}

fn push_top_existing_candidate(top: &mut Vec<LocalReuseCandidate>, candidate: LocalReuseCandidate) {
    if top.len() == LOCAL_REUSE_CANDIDATE_LIMIT
        && top
            .last()
            .is_some_and(|last| last.candidate.score >= candidate.candidate.score)
    {
        return;
    }

    top.push(candidate);
    top.sort_by(|left, right| right.candidate.score.total_cmp(&left.candidate.score));
    top.truncate(LOCAL_REUSE_CANDIDATE_LIMIT);
}

fn classify_candidates(
    youtube: &YoutubeTrack,
    candidates: Vec<ScoredCandidate>,
) -> CandidateDecision {
    let top = candidates.first();
    let second = candidates.get(1);
    if let Some(top) = top {
        if let Some(candidate) = text_confident_auto_match(youtube, &candidates) {
            return CandidateDecision::Accept {
                candidate,
                opencode_reason: None,
            };
        }
        if let Some(candidate) = close_duration_text_artist_auto_match(youtube, &candidates) {
            return CandidateDecision::Accept {
                candidate,
                opencode_reason: None,
            };
        }
        let gap = second
            .map(|candidate| top.score - candidate.score)
            .unwrap_or(100.0);
        if search_result_auto_match_is_safe(youtube, top) && gap >= AUTO_ACCEPT_GAP {
            return CandidateDecision::Accept {
                candidate: top.clone(),
                opencode_reason: None,
            };
        }
        if top.score >= DURATION_TIE_SCORE
            && let Some(candidate) = best_same_duration_tie(youtube, &candidates)
        {
            return CandidateDecision::Accept {
                candidate,
                opencode_reason: None,
            };
        }
    }

    CandidateDecision::Prompt {
        candidates,
        opencode_rejection: None,
    }
}

fn text_confident_auto_match(
    youtube: &YoutubeTrack,
    candidates: &[ScoredCandidate],
) -> Option<ScoredCandidate> {
    let top = candidates.first()?;
    if title_similarity(youtube, &top.track) < 0.96
        || primary_artist_similarity(&youtube.artists, &top.track.artists) < 0.96
        || marker_mismatch_count(&youtube.title, &top.track.title) != 0
    {
        return None;
    }

    if has_similar_same_artist_alternative(youtube, candidates, &top.track.uri) {
        return None;
    }

    Some(top.clone())
}

fn close_duration_text_artist_auto_match(
    youtube: &YoutubeTrack,
    candidates: &[ScoredCandidate],
) -> Option<ScoredCandidate> {
    let top_score = candidates.first()?.score;
    if top_score < CLOSE_DURATION_TEXT_ARTIST_SCORE {
        return None;
    }

    candidates
        .iter()
        .filter(|candidate| (candidate.score - top_score).abs() <= EQUAL_SCORE_EPSILON)
        .filter(|candidate| title_similarity(youtube, &candidate.track) >= 0.96)
        .filter(|candidate| {
            artist_set_similarity(&youtube.artists, &candidate.track.artists)
                >= ARTIST_SET_AUTO_THRESHOLD
        })
        .filter(|candidate| marker_mismatch_count(&youtube.title, &candidate.track.title) == 0)
        .filter(|candidate| {
            duration_delta_seconds(youtube.duration_ms, candidate.track.duration_ms)
                .map(|delta| delta <= CLOSE_DURATION_TEXT_ARTIST_SECONDS)
                .unwrap_or(false)
        })
        .min_by(|left, right| duration_then_title_len(youtube, left, right))
        .cloned()
}

fn has_similar_same_artist_alternative(
    youtube: &YoutubeTrack,
    candidates: &[ScoredCandidate],
    selected_uri: &str,
) -> bool {
    candidates.iter().any(|candidate| {
        candidate.track.uri != selected_uri
            && primary_artist_similarity(&youtube.artists, &candidate.track.artists)
                >= PRIMARY_ARTIST_AUTO_THRESHOLD
            && title_similarity(youtube, &candidate.track) >= 0.75
    })
}

fn best_same_duration_tie(
    youtube: &YoutubeTrack,
    candidates: &[ScoredCandidate],
) -> Option<ScoredCandidate> {
    let top_score = candidates
        .first()
        .map(|candidate| candidate.score)
        .unwrap_or(0.0);
    candidates
        .iter()
        .filter(|candidate| (candidate.score - top_score).abs() <= EQUAL_SCORE_EPSILON)
        .filter(|candidate| close_duration(youtube.duration_ms, candidate.track.duration_ms))
        .filter(|candidate| search_result_auto_match_is_safe(youtube, candidate))
        .min_by(|left, right| duration_then_title_len(youtube, left, right))
        .cloned()
}

fn duration_then_title_len(
    youtube: &YoutubeTrack,
    left: &ScoredCandidate,
    right: &ScoredCandidate,
) -> std::cmp::Ordering {
    let delta = |candidate: &ScoredCandidate| {
        duration_delta_seconds(youtube.duration_ms, candidate.track.duration_ms)
            .unwrap_or(f64::INFINITY)
    };
    delta(left).total_cmp(&delta(right)).then_with(|| {
        normalize(&left.track.title)
            .len()
            .cmp(&normalize(&right.track.title).len())
    })
}

fn search_result_auto_match_is_safe(youtube: &YoutubeTrack, candidate: &ScoredCandidate) -> bool {
    candidate.score >= AUTO_ACCEPT_SCORE
        && title_similarity(youtube, &candidate.track) >= TITLE_AUTO_THRESHOLD
        && primary_artist_similarity(&youtube.artists, &candidate.track.artists)
            >= PRIMARY_ARTIST_AUTO_THRESHOLD
        && durations_compatible(
            youtube.duration_ms,
            candidate.track.duration_ms,
            DURATION_COMPATIBLE_SECONDS,
        )
        && marker_mismatch_count(&youtube.title, &candidate.track.title) == 0
}

fn existing_playlist_match_is_safe(youtube: &YoutubeTrack, candidate: &ScoredCandidate) -> bool {
    candidate.score >= EXISTING_PLAYLIST_MIN_SCORE
        && title_similarity(youtube, &candidate.track) >= EXISTING_PLAYLIST_TITLE_THRESHOLD
        && (primary_artist_similarity(&youtube.artists, &candidate.track.artists)
            >= EXISTING_PLAYLIST_ARTIST_THRESHOLD
            || artist_set_similarity(&youtube.artists, &candidate.track.artists)
                >= EXISTING_PLAYLIST_ARTIST_SET_THRESHOLD)
        && durations_compatible(
            youtube.duration_ms,
            candidate.track.duration_ms,
            EXISTING_PLAYLIST_DURATION_SECONDS,
        )
}

fn existing_sequence_match_is_safe(youtube: &YoutubeTrack, candidate: &ScoredCandidate) -> bool {
    candidate.score >= EXISTING_SEQUENCE_MIN_SCORE
        && (title_similarity(youtube, &candidate.track) >= EXISTING_SEQUENCE_TITLE_THRESHOLD
            || cleaned_title_similarity(youtube, &candidate.track) >= 0.82)
        && durations_compatible(
            youtube.duration_ms,
            candidate.track.duration_ms,
            EXISTING_SEQUENCE_DURATION_SECONDS,
        )
}

fn same_index_existing_match_is_safe(youtube: &YoutubeTrack, candidate: &ScoredCandidate) -> bool {
    title_similarity(youtube, &candidate.track) >= EXISTING_INDEX_TITLE_THRESHOLD
        && primary_artist_similarity(&youtube.artists, &candidate.track.artists)
            >= EXISTING_INDEX_ARTIST_THRESHOLD
        && durations_compatible(
            youtube.duration_ms,
            candidate.track.duration_ms,
            EXISTING_INDEX_DURATION_SECONDS,
        )
}

fn title_similarity(youtube: &YoutubeTrack, spotify: &SpotifyTrack) -> f64 {
    similarity(&youtube.title, &spotify.title)
}

fn cleaned_title_similarity(youtube: &YoutubeTrack, spotify: &SpotifyTrack) -> f64 {
    let youtube_title = search_clean_text(&strip_leading_artist_prefixes(
        &remove_presentation_labels(&remove_bracketed_segments(&clean_search_title(
            &youtube.title,
        ))),
        &youtube.artists,
    ));
    let spotify_title = search_clean_text(&remove_presentation_labels(&remove_bracketed_segments(
        &clean_search_title(&spotify.title),
    )));
    similarity(&youtube_title, &spotify_title)
}

fn close_duration(youtube_ms: Option<u64>, spotify_ms: Option<u64>) -> bool {
    duration_delta_seconds(youtube_ms, spotify_ms)
        .map(|delta| delta <= DURATION_AUTO_TOLERANCE_SECONDS)
        .unwrap_or(false)
}

fn duration_delta_seconds(youtube_ms: Option<u64>, spotify_ms: Option<u64>) -> Option<f64> {
    let (Some(youtube_ms), Some(spotify_ms)) = (youtube_ms, spotify_ms) else {
        return None;
    };
    Some(youtube_ms.abs_diff(spotify_ms) as f64 / 1000.0)
}

fn apply_cached_decision(
    result: &mut MatchResult,
    youtube: YoutubeTrack,
    cached: Option<ScoredCandidate>,
) {
    match cached {
        Some(candidate) => push_match(result, youtube, candidate),
        None => push_skip(
            result,
            youtube,
            "duplicate of previously skipped track".to_string(),
        ),
    }
}

fn opencode_skip_reason(reason: Option<String>) -> String {
    format!(
        "skipped by --opencode: {}",
        reason.unwrap_or_else(|| "opencode did not choose a track".to_string())
    )
}

fn set_task_message(progress: &Progress, task: &ProgressBar, stage: &str, youtube: &YoutubeTrack) {
    if progress.rendering_paused() {
        return;
    }
    task.set_message(format!(
        "{stage} #{} {} - {}",
        youtube.index + 1,
        youtube.artist_display(),
        youtube.title
    ));
}

fn durations_compatible(
    youtube_ms: Option<u64>,
    spotify_ms: Option<u64>,
    max_delta_seconds: f64,
) -> bool {
    duration_delta_seconds(youtube_ms, spotify_ms)
        .map(|delta| delta <= max_delta_seconds)
        .unwrap_or(true)
}

fn push_match(result: &mut MatchResult, youtube: YoutubeTrack, candidate: ScoredCandidate) {
    result.matched.push(MatchedTrack {
        youtube,
        spotify: candidate.track,
        score: candidate.score,
    });
}

fn push_opencode_resolved(
    result: &mut MatchResult,
    youtube: &YoutubeTrack,
    candidate: &ScoredCandidate,
    reason: Option<String>,
) {
    result.opencode_resolved.push(OpencodeResolvedTrack {
        youtube: youtube.clone(),
        spotify: candidate.track.clone(),
        reason,
    });
}

fn push_skip(result: &mut MatchResult, youtube: YoutubeTrack, reason: String) {
    let skipped = SkippedTrack { youtube, reason };
    print_skip(&skipped);
    result.skipped.push(skipped);
}

async fn prompt_manual_or_skip(
    spotify: &SpotifyClient,
    youtube: &YoutubeTrack,
    candidates: Vec<ScoredCandidate>,
) -> Result<Option<ScoredCandidate>> {
    loop {
        let prompt = format_youtube_prompt(youtube);

        let mut items = candidates.iter().map(format_candidate).collect::<Vec<_>>();
        items.push("Search Spotify with custom query".to_string());
        items.push("Skip".to_string());

        let selection = select_item(&prompt, &items, 0)?;

        if selection < candidates.len() {
            return Ok(Some(candidates[selection].clone()));
        }

        if selection == candidates.len() {
            let query: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Custom Spotify search query")
                .with_initial_text(youtube.search_seed())
                .interact_text()?;
            let tracks = spotify.search_tracks(&query, 10).await?;
            let mut manual = tracks
                .into_iter()
                .map(|track| score_candidate(youtube, track))
                .collect::<Vec<_>>();
            manual.sort_by(|a, b| b.score.total_cmp(&a.score));
            if manual.is_empty() {
                println!("No Spotify tracks found for manual query: {query}");
                continue;
            }

            let mut manual_items = manual.iter().map(format_candidate).collect::<Vec<_>>();
            manual_items.push("Back".to_string());
            manual_items.push("Skip".to_string());
            let manual_selection = select_item(
                &format!("Custom search results for #{}: {query}", youtube.index + 1),
                &manual_items,
                0,
            )?;
            if manual_selection < manual.len() {
                return Ok(Some(manual[manual_selection].clone()));
            }
            if manual_selection == manual.len() {
                continue;
            }
            return Ok(None);
        }

        return Ok(None);
    }
}

fn select_item(prompt: &str, items: &[String], default: usize) -> Result<usize> {
    Ok(Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .items(items)
        .default(default.min(items.len().saturating_sub(1)))
        .interact()?)
}

fn format_youtube_prompt(youtube: &YoutubeTrack) -> String {
    let album = youtube.album.as_deref().unwrap_or("unknown album");
    let duration = youtube
        .duration_ms
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    format!(
        "Ambiguous match for #{}\nYouTube: {} - {}\nAlbum: {} | Duration: {} | Video ID: {} | Thumbnails: {}\nChoose Spotify track",
        youtube.index + 1,
        youtube.artist_display(),
        youtube.title,
        album,
        duration,
        youtube.video_id,
        youtube.thumbnails.len()
    )
}

fn score_candidate(youtube: &YoutubeTrack, spotify: SpotifyTrack) -> ScoredCandidate {
    let title_score = similarity(&youtube.title, &spotify.title) * 48.0;
    let primary_artist_score = primary_artist_similarity(&youtube.artists, &spotify.artists) * 30.0;
    let secondary_artist_score =
        secondary_artist_similarity(&youtube.artists, &spotify.artists) * 4.0;
    let album_score = match (&youtube.album, &spotify.album) {
        (Some(yt), Some(sp)) if !yt.is_empty() && !sp.is_empty() => similarity(yt, sp) * 8.0,
        _ => 0.0,
    };
    let duration_score = duration_score(youtube.duration_ms, spotify.duration_ms) * 10.0;
    let marker_penalty = marker_mismatch_penalty(&youtube.title, &spotify.title);
    let score = (title_score
        + primary_artist_score
        + secondary_artist_score
        + album_score
        + duration_score
        - marker_penalty)
        .clamp(0.0, 100.0);

    ScoredCandidate {
        track: spotify,
        score,
        reason: format!(
            "title {:.0}, primary artist {:.0}, other artists {:.0}, album {:.0}, duration {:.0}, penalty {:.0}",
            title_score,
            primary_artist_score,
            secondary_artist_score,
            album_score,
            duration_score,
            marker_penalty
        ),
    }
}

fn similarity(left: &str, right: &str) -> f64 {
    let left = normalize(left);
    let right = normalize(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    jaro_winkler(&left, &right).max(token_overlap_similarity(&left, &right))
}

fn token_overlap_similarity(left: &str, right: &str) -> f64 {
    let left_tokens = left.split_whitespace().collect::<HashSet<_>>();
    let right_tokens = right.split_whitespace().collect::<HashSet<_>>();
    if left_tokens.is_empty() || right_tokens.is_empty() {
        return 0.0;
    }

    let shared = left_tokens.intersection(&right_tokens).count() as f64;
    let left_coverage = shared / left_tokens.len() as f64;
    let right_coverage = shared / right_tokens.len() as f64;
    (left_coverage + right_coverage) / 2.0
}

fn primary_artist_similarity(youtube: &[String], spotify: &[String]) -> f64 {
    let (Some(youtube), Some(spotify)) = (youtube.first(), spotify.first()) else {
        return 0.0;
    };
    similarity(youtube, spotify)
}

fn secondary_artist_similarity(youtube: &[String], spotify: &[String]) -> f64 {
    if youtube.len() < 2 || spotify.is_empty() {
        return 0.0;
    }
    artist_similarity(&youtube[1..], spotify)
}

fn artist_set_similarity(youtube: &[String], spotify: &[String]) -> f64 {
    if youtube.is_empty() || spotify.is_empty() {
        return 0.0;
    }

    let yt_coverage = average_best_artist_similarity(youtube, spotify);
    let sp_coverage = average_best_artist_similarity(spotify, youtube);
    (yt_coverage + sp_coverage) / 2.0
}

fn average_best_artist_similarity(left: &[String], right: &[String]) -> f64 {
    left.iter()
        .map(|artist| {
            right
                .iter()
                .map(|other| similarity(artist, other))
                .fold(0.0, f64::max)
        })
        .sum::<f64>()
        / left.len() as f64
}

fn artist_similarity(youtube: &[String], spotify: &[String]) -> f64 {
    if youtube.is_empty() || spotify.is_empty() {
        return 0.0;
    }
    youtube
        .iter()
        .flat_map(|yt| spotify.iter().map(move |sp| similarity(yt, sp)))
        .fold(0.0, f64::max)
}

fn duration_score(youtube_ms: Option<u64>, spotify_ms: Option<u64>) -> f64 {
    let Some(delta_seconds) = duration_delta_seconds(youtube_ms, spotify_ms) else {
        return 0.5;
    };

    (1.0 - (delta_seconds / 20.0)).clamp(0.0, 1.0)
}

fn marker_mismatch_penalty(youtube_title: &str, spotify_title: &str) -> f64 {
    marker_mismatch_count(youtube_title, spotify_title) as f64 * 6.0
}

fn marker_mismatch_count(youtube_title: &str, spotify_title: &str) -> usize {
    [
        "live",
        "remaster",
        "acoustic",
        "instrumental",
        "cover",
        "karaoke",
        "remix",
        "edit",
        "radio",
        "extended",
        "demo",
        "mono",
        "stereo",
        "sped",
        "slowed",
        "nightcore",
        "version",
        "alternate",
    ]
    .into_iter()
    .filter(|marker| {
        contains_marker(youtube_title, marker) != contains_marker(spotify_title, marker)
    })
    .count()
}

fn contains_marker(text: &str, marker: &str) -> bool {
    normalize(text)
        .split_whitespace()
        .any(|token| token == marker || token.starts_with(&format!("{marker}ed")))
}

pub fn normalize(input: &str) -> String {
    static CREDIT_SEGMENT: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?i)((\(|\[)\s*with\b|(\(|\[|\b)(feat\.?|ft\.?|featuring|prod\.?)).*$")
            .expect("credit regex is valid")
    });
    static NOISE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?i)\b(official|music video|video|lyrics?|lyric video|audio|visualizer|hd|4k)\b",
        )
        .expect("noise regex is valid")
    });
    static PUNCT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"[^\p{L}\p{N}]+").expect("punct regex is valid"));
    static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").expect("spaces regex is valid"));

    let decomposed = ascii_fold(input).to_lowercase();
    let without_credits = CREDIT_SEGMENT.replace_all(&decomposed, " ");
    let without_noise = NOISE.replace_all(&without_credits, " ");
    let folded = PUNCT.replace_all(&without_noise, " ");
    SPACES.replace_all(folded.trim(), " ").to_string()
}

fn ascii_fold(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.nfkd().filter(|ch| !is_combining_mark(*ch)) {
        match ch {
            'æ' | 'ǽ' | 'Æ' | 'Ǽ' => output.push_str("ae"),
            'œ' | 'Œ' => output.push_str("oe"),
            'ß' => output.push_str("ss"),
            'ø' | 'Ø' => output.push('o'),
            'ð' | 'Ð' | 'đ' | 'Đ' => output.push('d'),
            'þ' | 'Þ' => output.push_str("th"),
            'ł' | 'Ł' => output.push('l'),
            'ħ' | 'Ħ' => output.push('h'),
            'ı' => output.push('i'),
            'Ŋ' | 'ŋ' => output.push('n'),
            '’' | '‘' | '‚' | '‛' | '`' | '´' => output.push('\''),
            '“' | '”' | '„' | '‟' => output.push('"'),
            '‐' | '‑' | '‒' | '–' | '—' | '―' | '−' => output.push('-'),
            '•' | '·' | '∙' | '●' | '★' | '☆' => output.push(' '),
            ch if ch.is_ascii() => output.push(ch),
            ch if ch.is_alphanumeric() => output.push(ch),
            ch if ch.is_whitespace() => output.push(' '),
            _ => {}
        }
    }
    output
}

fn decision_cache_key(youtube: &YoutubeTrack) -> String {
    format!(
        "{}|{}|{}",
        youtube
            .artists
            .iter()
            .map(|artist| normalize(artist))
            .collect::<Vec<_>>()
            .join(","),
        normalize(&youtube.title),
        youtube.duration_ms.unwrap_or_default()
    )
}

fn format_candidate(candidate: &ScoredCandidate) -> String {
    let track = &candidate.track;
    let duration = track
        .duration_ms
        .map(format_duration)
        .unwrap_or_else(|| "?:??".to_string());
    let album = track.album.as_deref().unwrap_or("unknown album");
    format!(
        "{:.1} | {} - {} | {} | {} | {}",
        candidate.score,
        track.artist_display(),
        track.title,
        album,
        duration,
        candidate.reason
    )
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn print_skip(skipped: &SkippedTrack) {
    eprintln!(
        "ERROR: skipped #{} {} - {}: {}",
        skipped.youtube.index + 1,
        skipped.youtube.artist_display(),
        skipped.youtube.title,
        skipped.reason
    );
}

#[cfg(test)]
mod tests {
    use crate::model::SpotifyPlaylistItem;

    use super::*;

    fn yt(title: &str, artist: &str, duration_ms: Option<u64>) -> YoutubeTrack {
        YoutubeTrack {
            index: 0,
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: Some("Album".to_string()),
            duration_ms,
            video_id: "video".to_string(),
            thumbnails: Vec::new(),
        }
    }

    fn sp_with_uri(uri: &str, title: &str, artist: &str, duration_ms: Option<u64>) -> SpotifyTrack {
        SpotifyTrack {
            uri: uri.to_string(),
            title: title.to_string(),
            artists: vec![artist.to_string()],
            album: Some("Album".to_string()),
            duration_ms,
            image_url: None,
        }
    }

    fn sp(title: &str, artist: &str, duration_ms: Option<u64>) -> SpotifyTrack {
        sp_with_uri("spotify:track:1", title, artist, duration_ms)
    }

    fn snapshot(tracks: Vec<SpotifyTrack>) -> PlaylistSnapshot {
        PlaylistSnapshot {
            uri: "spotify:playlist:p".to_string(),
            name: "Playlist".to_string(),
            items: tracks
                .into_iter()
                .enumerate()
                .map(|(index, track)| SpotifyPlaylistItem {
                    uid: format!("uid-{index}"),
                    track,
                })
                .collect(),
        }
    }

    #[test]
    fn normalizes_noise_and_accents() {
        assert_eq!(normalize("Café (Official Video) ft. Someone"), "cafe");
        assert_eq!(normalize("Yoroï (with Thomas Bangalter)"), "yoroi");
        assert_eq!(normalize("half•alive – déjà vu 😵‍💫"), "half alive deja vu");
    }

    #[test]
    fn search_queries_include_feature_stripped_title() {
        let youtube = yt("Yoroï (feat. Thomas Bangalter)", "Orelsan", Some(199000));

        let queries = search_queries(&youtube);

        assert!(queries.contains(&"orelsan yoroi".to_string()));
        assert!(queries.contains(&"yoroi".to_string()));
    }

    #[test]
    fn search_queries_strip_presentation_brackets_and_duplicate_artist_prefix() {
        let youtube = yt(
            "Des Rocs - Suicide Romantics (from \"Des Rocs Alive\")",
            "Des Rocs",
            Some(180000),
        );

        let queries = search_queries(&youtube);

        assert!(queries.contains(&"des rocs suicide romantics".to_string()));
        assert!(queries.contains(&"suicide romantics".to_string()));
        assert!(!queries.iter().any(|query| query.contains("from")));
    }

    #[test]
    fn search_queries_use_all_artists_and_ascii_fold() {
        let mut youtube = yt("Fever", "Elvis Presley", Some(265000));
        youtube.artists.push("Michael Bublé".to_string());

        let queries = search_queries(&youtube);

        assert!(queries.contains(&"elvis presley michael buble fever".to_string()));
    }

    #[test]
    fn scores_close_match_high() {
        let scored = score_candidate(
            &yt("Song", "Artist", Some(180000)),
            sp("Song", "Artist", Some(181000)),
        );
        assert!(scored.score > AUTO_ACCEPT_SCORE);
    }

    #[test]
    fn duration_mismatch_penalizes() {
        let scored = score_candidate(
            &yt("Song", "Artist", Some(180000)),
            sp("Song", "Artist", Some(260000)),
        );
        assert!(scored.score < 92.0);
    }

    #[test]
    fn two_second_duration_diff_auto_accepts_ambiguous_top() {
        let youtube = yt("Song", "Artist", Some(180000));
        let candidates = vec![
            ScoredCandidate {
                track: sp_with_uri("spotify:track:1", "Song - Single", "Artist", Some(182000)),
                score: 88.0,
                reason: "test".to_string(),
            },
            ScoredCandidate {
                track: sp_with_uri("spotify:track:2", "Song", "Artist", Some(179000)),
                score: 86.0,
                reason: "test".to_string(),
            },
        ];

        match classify_candidates(&youtube, candidates) {
            CandidateDecision::Accept { candidate, .. } => {
                assert_eq!(candidate.track.uri, "spotify:track:1");
            }
            CandidateDecision::Prompt { .. } => {
                panic!("close-duration high match should auto-accept")
            }
        }
    }

    #[test]
    fn closer_duration_scores_higher() {
        let youtube = yt("Song", "Artist", Some(180000));
        let close = score_candidate(&youtube, sp("Song", "Artist", Some(181000)));
        let farther = score_candidate(&youtube, sp("Song", "Artist", Some(187000)));

        assert!(close.score > farther.score);
    }

    #[test]
    fn same_duration_equal_strength_prefers_shorter_title() {
        let youtube = yt("Song", "Artist", Some(180000));
        let candidates = vec![
            ScoredCandidate {
                track: sp_with_uri(
                    "spotify:track:long",
                    "Song - 2011 Remastered Version",
                    "Artist",
                    Some(180000),
                ),
                score: 88.0,
                reason: "test".to_string(),
            },
            ScoredCandidate {
                track: sp_with_uri("spotify:track:short", "Song", "Artist", Some(180000)),
                score: 87.8,
                reason: "test".to_string(),
            },
        ];

        match classify_candidates(&youtube, candidates) {
            CandidateDecision::Accept { candidate, .. } => {
                assert_eq!(candidate.track.uri, "spotify:track:short");
            }
            CandidateDecision::Prompt { .. } => panic!("same-duration tie should auto-accept"),
        }
    }

    #[test]
    fn featured_artist_only_match_does_not_auto_accept() {
        let youtube = yt("Song", "Primary Artist", Some(180000));
        let mut track = sp("Song", "Featured Artist", Some(180000));
        track.artists.push("Primary Artist".to_string());
        let candidate = score_candidate(&youtube, track);

        match classify_candidates(&youtube, vec![candidate]) {
            CandidateDecision::Accept { .. } => {
                panic!("secondary artist match should not auto-accept")
            }
            CandidateDecision::Prompt { .. } => {}
        }
    }

    #[test]
    fn variant_marker_mismatch_does_not_auto_accept() {
        let youtube = yt("Song", "Artist", Some(180000));
        let candidate = score_candidate(&youtube, sp("Song - Live", "Artist", Some(180000)));

        match classify_candidates(&youtube, vec![candidate]) {
            CandidateDecision::Accept { .. } => panic!("variant marker mismatch should prompt"),
            CandidateDecision::Prompt { .. } => {}
        }
    }

    #[test]
    fn exact_title_and_artist_auto_accepts_even_with_bad_duration() {
        let youtube = yt("Drag Path", "twenty one pilots", Some(293000));
        let mut candidates = vec![
            score_candidate(
                &youtube,
                sp_with_uri(
                    "spotify:track:drag-path",
                    "Drag Path",
                    "Twenty One Pilots",
                    Some(224000),
                ),
            ),
            score_candidate(
                &youtube,
                sp_with_uri(
                    "spotify:track:wrong-artist",
                    "Drag Path",
                    "Silverkrow",
                    Some(293000),
                ),
            ),
        ];
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

        match classify_candidates(&youtube, candidates) {
            CandidateDecision::Accept { candidate, .. } => {
                assert_eq!(candidate.track.uri, "spotify:track:drag-path");
            }
            CandidateDecision::Prompt { .. } => {
                panic!("exact title and only matching artist should auto-accept")
            }
        }
    }

    #[test]
    fn exact_title_and_artist_prompts_when_same_artist_alternative_exists() {
        let youtube = yt("Song", "Artist", Some(180000));
        let mut candidates = vec![
            score_candidate(
                &youtube,
                sp_with_uri("spotify:track:a", "Song", "Artist", Some(260000)),
            ),
            score_candidate(
                &youtube,
                sp_with_uri(
                    "spotify:track:b",
                    "Song - Alternate",
                    "Artist",
                    Some(180000),
                ),
            ),
        ];
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

        match classify_candidates(&youtube, candidates) {
            CandidateDecision::Accept { .. } => {
                panic!("same-artist alternatives should stay ambiguous")
            }
            CandidateDecision::Prompt { .. } => {}
        }
    }

    #[test]
    fn exact_title_artist_set_and_one_second_duration_auto_accepts() {
        let youtube = YoutubeTrack {
            index: 53,
            title: "ORATORES".to_string(),
            artists: vec![
                "Apashe".to_string(),
                "Vladimir Cauchemar".to_string(),
                "Ruti".to_string(),
            ],
            album: None,
            duration_ms: Some(163000),
            video_id: "video".to_string(),
            thumbnails: Vec::new(),
        };
        let mut candidates = vec![
            score_candidate(
                &youtube,
                SpotifyTrack {
                    uri: "spotify:track:one-second-off".to_string(),
                    title: "ORATORES".to_string(),
                    artists: vec![
                        "Vladimir Cauchemar".to_string(),
                        "Apashe".to_string(),
                        "Ruti".to_string(),
                    ],
                    album: Some("ORATORES, BELLATORES, LABORATORES".to_string()),
                    duration_ms: Some(162000),
                    image_url: None,
                },
            ),
            score_candidate(
                &youtube,
                SpotifyTrack {
                    uri: "spotify:track:exact".to_string(),
                    title: "ORATORES".to_string(),
                    artists: vec![
                        "Vladimir Cauchemar".to_string(),
                        "Apashe".to_string(),
                        "Ruti".to_string(),
                    ],
                    album: None,
                    duration_ms: Some(163000),
                    image_url: None,
                },
            ),
            score_candidate(
                &youtube,
                SpotifyTrack {
                    uri: "spotify:track:wrong-title".to_string(),
                    title: "LABORATORES".to_string(),
                    artists: vec!["Vladimir Cauchemar".to_string(), "Apashe".to_string()],
                    album: Some("ORATORES, BELLATORES, LABORATORES".to_string()),
                    duration_ms: Some(121000),
                    image_url: None,
                },
            ),
        ];
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));

        match classify_candidates(&youtube, candidates) {
            CandidateDecision::Accept { candidate, .. } => {
                assert_eq!(candidate.track.uri, "spotify:track:exact");
            }
            CandidateDecision::Prompt { .. } => {
                panic!("close exact title/artist set should auto-accept")
            }
        }
    }

    #[tokio::test]
    async fn same_length_existing_playlist_reuses_same_index_track() {
        let youtube = vec![yt("Song", "Artist", Some(180000))];
        let current = snapshot(vec![sp("Song - Radio Edit", "Artist", Some(181000))]);
        let mut used_existing = HashSet::new();
        let progress = Progress::new();

        let matches =
            reuse_existing_matches(&youtube, &current, &mut used_existing, &progress).await;
        progress.finish();

        assert_eq!(
            matches[0].as_ref().unwrap().track.title,
            "Song - Radio Edit"
        );
    }
}
