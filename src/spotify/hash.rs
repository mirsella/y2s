use std::collections::{HashMap, HashSet};

use regex::Regex;
use reqwest::Url;

use crate::error::{AppError, Result};

const REQUIRED_OPERATIONS: &[&str] = &[
    "fetchPlaylist",
    "libraryV3",
    "addToPlaylist",
    "removeFromPlaylist",
];
const OPTIONAL_OPERATIONS: &[&str] = &["searchSuggestions", "assistedCurationSearch"];

pub async fn discover_hashes(
    http: &reqwest::Client,
    html: &str,
) -> Result<HashMap<String, String>> {
    let scripts = script_urls(html)?;
    if scripts.is_empty() {
        return Err(AppError::Spotify(
            "Spotify landing page did not expose any JavaScript bundle URLs for GraphQL hash discovery".to_string(),
        ));
    }

    let mut hashes = HashMap::new();
    for script in scripts {
        if REQUIRED_OPERATIONS
            .iter()
            .all(|op| hashes.contains_key(*op))
            && OPTIONAL_OPERATIONS
                .iter()
                .all(|op| hashes.contains_key(*op))
        {
            break;
        }

        let response = http.get(script.clone()).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            continue;
        }

        for operation in REQUIRED_OPERATIONS.iter().chain(OPTIONAL_OPERATIONS) {
            if hashes.contains_key(*operation) {
                continue;
            }
            if let Some(hash) = find_operation_hash(&body, operation) {
                hashes.insert((*operation).to_string(), hash);
            }
        }
    }

    let missing = REQUIRED_OPERATIONS
        .iter()
        .filter(|operation| !hashes.contains_key(**operation))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(AppError::Spotify(format!(
            "could not discover Spotify GraphQL persisted hashes for: {}; private endpoint shape likely changed",
            missing.join(", ")
        )));
    }

    Ok(hashes)
}

fn script_urls(html: &str) -> Result<Vec<Url>> {
    let base = Url::parse("https://open.spotify.com/")?;
    let re = Regex::new(r#"<script[^>]+src=["'](?P<src>[^"']+\.js)["']"#)
        .expect("script src regex is valid");
    let mut seen = HashSet::new();
    let mut urls = Vec::new();
    for captures in re.captures_iter(html) {
        let src = &captures["src"];
        let url = base.join(src)?;
        if seen.insert(url.clone()) {
            urls.push(url);
        }
    }
    Ok(urls)
}

fn find_operation_hash(bundle: &str, operation: &str) -> Option<String> {
    let escaped = regex::escape(operation);
    let hash = r#"([a-f0-9]{64})"#;
    let patterns = [
        format!(r#"{escaped}[^{{}}]{{0,1200}}sha256Hash["']?\s*[:=]\s*["']{hash}["']"#),
        format!(r#"sha256Hash["']?\s*[:=]\s*["']{hash}["'][^{{}}]{{0,1200}}{escaped}"#),
        format!(r#"["']{escaped}["']\s*,\s*["'](?:query|mutation)["'][^a-f0-9]{{0,300}}{hash}"#),
        format!(r#"["']{escaped}["'][^a-f0-9]{{0,300}}{hash}"#),
    ];

    for pattern in patterns {
        let re = Regex::new(&pattern).expect("operation hash regex is valid");
        if let Some(captures) = re.captures(bundle) {
            return captures.get(1).map(|m| m.as_str().to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_operation_hash_after_name() {
        let bundle = r#"foo "fetchPlaylist" bar sha256Hash:"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef""#;
        assert_eq!(
            find_operation_hash(bundle, "fetchPlaylist").unwrap(),
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
    }
}
