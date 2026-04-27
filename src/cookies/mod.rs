use std::{collections::HashMap, fs, sync::Arc};

use cookie_scoop::{BrowserName, Cookie, CookieMode, GetCookiesOptions, get_cookies};
use dialoguer::{Select, theme::ColorfulTheme};
use reqwest::{Url, cookie::Jar, header::USER_AGENT};
use serde_json::Value;

use crate::{
    cli::Cli,
    error::{AppError, Result},
    progress::Progress,
};

const COOKIE_SCOOP_BROWSERS: &[BrowserName] = &[
    BrowserName::Chrome,
    BrowserName::Firefox,
    BrowserName::Zen,
    BrowserName::Helium,
];
const SPOTIFY_COOKIE_DOMAINS: &[&str] =
    &["spotify.com", "open.spotify.com", "accounts.spotify.com"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
}

impl BrowserCookie {
    pub fn spotify_default(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            domain: ".spotify.com".to_string(),
            path: "/".to_string(),
            secure: true,
        }
    }
}

#[derive(Debug, Clone)]
struct CookieCandidate {
    label: String,
    cookies: Vec<BrowserCookie>,
}

pub async fn load_cookies(cli: &Cli, progress: &Progress) -> Result<Vec<BrowserCookie>> {
    if let Some(path) = &cli.spotify_cookie_file {
        let contents = fs::read_to_string(path)?;
        let cookies = parse_cookie_file(&contents)?;
        let source = format!("cookie file {}", path.display());
        ensure_spotify_cookie_shape(&cookies, &source)?;
        return Ok(cookies);
    }

    let spinner = progress.spinner("discovering browser Spotify cookies");
    let source = cli
        .browser_profile
        .as_ref()
        .map(|profile| format!("browser profile {}", profile.display()))
        .unwrap_or_else(|| "browser cookie discovery".to_string());
    let candidates = browser_cookie_candidates(cli.browser_profile.as_deref()).await;
    spinner.set_message(format!(
        "validating {} browser cookie candidate(s)",
        candidates.len()
    ));
    let validated = validate_candidates(candidates).await;
    spinner.finish_and_clear();

    choose_cookie_candidate(validated, &source)
}

pub fn parse_cookie_file(contents: &str) -> Result<Vec<BrowserCookie>> {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        return Err(AppError::Cookie("cookie file is empty".to_string()));
    }

    if trimmed.starts_with('{') {
        return parse_json_cookie_map(trimmed);
    }

    if trimmed.starts_with("Cookie:") || (trimmed.contains('=') && trimmed.contains(';')) {
        return parse_raw_cookie_header(trimmed);
    }

    parse_netscape_cookie_jar(trimmed)
}

pub fn cookie_jar(cookies: &[BrowserCookie]) -> Result<Arc<Jar>> {
    let jar = Arc::new(Jar::default());
    for cookie in cookies {
        add_cookie_to_jar(&jar, cookie)?;
    }
    Ok(jar)
}

pub fn cookie_value<'a>(cookies: &'a [BrowserCookie], name: &str) -> Option<&'a str> {
    cookies
        .iter()
        .find(|cookie| cookie.name == name)
        .map(|cookie| cookie.value.as_str())
}

pub fn is_spotify_domain(domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    SPOTIFY_COOKIE_DOMAINS
        .iter()
        .any(|candidate| domain == *candidate || domain.ends_with(&format!(".{candidate}")))
}

fn parse_json_cookie_map(contents: &str) -> Result<Vec<BrowserCookie>> {
    let value: Value = serde_json::from_str(contents)?;
    let object = value
        .as_object()
        .ok_or_else(|| AppError::Cookie("JSON cookie input must be an object".to_string()))?;

    let mut cookies = Vec::new();
    for (name, value) in object {
        let value = value.as_str().ok_or_else(|| {
            AppError::Cookie(format!("JSON cookie value for {name} must be a string"))
        })?;
        cookies.push(BrowserCookie::spotify_default(name, value));
    }
    Ok(cookies)
}

fn parse_raw_cookie_header(contents: &str) -> Result<Vec<BrowserCookie>> {
    let header = contents
        .trim()
        .strip_prefix("Cookie:")
        .unwrap_or(contents)
        .trim();
    let cookies = header
        .split(';')
        .filter_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (!name.trim().is_empty())
                .then(|| BrowserCookie::spotify_default(name.trim(), value.trim()))
        })
        .collect::<Vec<_>>();

    if cookies.is_empty() {
        return Err(AppError::Cookie(
            "raw Cookie header did not contain any name=value pairs".to_string(),
        ));
    }
    Ok(cookies)
}

fn parse_netscape_cookie_jar(contents: &str) -> Result<Vec<BrowserCookie>> {
    let mut cookies = Vec::new();
    for (line_no, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') && !line.starts_with("#HttpOnly_") {
            continue;
        }

        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        let parts = line.split('\t').collect::<Vec<_>>();
        if parts.len() != 7 {
            return Err(AppError::Cookie(format!(
                "invalid Netscape cookie line {}: expected 7 tab-separated fields",
                line_no + 1
            )));
        }

        let domain = parts[0].to_string();
        if !is_spotify_domain(&domain) {
            continue;
        }
        let secure = parts[3].eq_ignore_ascii_case("TRUE");
        cookies.push(BrowserCookie {
            domain,
            path: parts[2].to_string(),
            secure,
            name: parts[5].to_string(),
            value: parts[6].to_string(),
        });
    }

    if cookies.is_empty() {
        return Err(AppError::Cookie(
            "Netscape cookie jar did not contain Spotify cookies".to_string(),
        ));
    }
    Ok(cookies)
}

async fn browser_cookie_candidates(profile: Option<&std::path::Path>) -> Vec<CookieCandidate> {
    let mut candidates = Vec::new();

    for &browser_name in COOKIE_SCOOP_BROWSERS {
        let mut options = spotify_cookie_options(browser_name);
        let label = if let Some(profile) = profile {
            options = options.browser_profile(browser_name, profile.to_string_lossy());
            format!("{browser_name} profile {}", profile.display())
        } else {
            format!("{browser_name} via cookie-scoop")
        };

        let result = get_cookies(options).await;
        let cookies = convert_cookie_scoop_cookies(result.cookies);
        if has_spotify_session_cookie(&cookies) {
            candidates.push(CookieCandidate { label, cookies });
        }
    }

    candidates
}

async fn validate_candidates(candidates: Vec<CookieCandidate>) -> Vec<CookieCandidate> {
    let mut validated = Vec::new();
    for candidate in candidates {
        match validate_candidate(&candidate).await {
            Ok(true) => validated.push(candidate),
            Ok(false) => {}
            Err(err) => eprintln!(
                "WARN: failed to validate Spotify cookie candidate {}: {err}",
                candidate.label
            ),
        }
    }

    validated
}

fn choose_cookie_candidate(
    mut candidates: Vec<CookieCandidate>,
    source: &str,
) -> Result<Vec<BrowserCookie>> {
    match candidates.len() {
        0 => Err(AppError::Cookie(format!(
            "{source} did not yield validated Spotify session cookies; pass --spotify-cookie-file or --browser-profile"
        ))),
        1 => Ok(candidates.remove(0).cookies),
        _ => {
            let labels = candidates
                .iter()
                .map(|candidate| candidate.label.as_str())
                .collect::<Vec<_>>();
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Choose Spotify browser session")
                .items(&labels)
                .default(0)
                .interact()?;
            Ok(candidates.remove(selection).cookies)
        }
    }
}

fn spotify_cookie_options(browser: BrowserName) -> GetCookiesOptions {
    GetCookiesOptions::new("https://open.spotify.com")
        .browsers(vec![browser])
        .mode(CookieMode::First)
}

fn convert_cookie_scoop_cookies(cookies: Vec<Cookie>) -> Vec<BrowserCookie> {
    dedupe_cookies(
        cookies
            .into_iter()
            .filter_map(|cookie| {
                let domain = cookie.domain.unwrap_or_else(|| ".spotify.com".to_string());
                is_spotify_domain(&domain).then(|| BrowserCookie {
                    name: cookie.name,
                    value: cookie.value,
                    domain,
                    path: cookie.path.unwrap_or_else(|| "/".to_string()),
                    secure: cookie.secure.unwrap_or(true),
                })
            })
            .collect(),
    )
}

fn ensure_spotify_cookie_shape(cookies: &[BrowserCookie], source: &str) -> Result<()> {
    if !has_spotify_session_cookie(cookies) {
        return Err(AppError::Cookie(format!(
            "{source} does not contain a Spotify session cookie such as sp_dc"
        )));
    }
    Ok(())
}

fn has_spotify_session_cookie(cookies: &[BrowserCookie]) -> bool {
    cookies
        .iter()
        .any(|cookie| cookie.name == "sp_dc" && !cookie.value.is_empty())
}

async fn validate_candidate(candidate: &CookieCandidate) -> Result<bool> {
    let jar = cookie_jar(&candidate.cookies)?;
    let client = reqwest::Client::builder().cookie_provider(jar).build()?;
    let response = client
        .get("https://www.spotify.com/api/account-settings/v1/profile")
        .header(
            USER_AGENT,
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123 Safari/537.36",
        )
        .send()
        .await?;

    if !response.status().is_success() {
        return Ok(false);
    }

    let json: Value = response.json().await?;
    Ok(json.get("username").and_then(Value::as_str).is_some()
        || json
            .pointer("/profile/username")
            .and_then(Value::as_str)
            .is_some())
}

fn add_cookie_to_jar(jar: &Jar, cookie: &BrowserCookie) -> Result<()> {
    let host = cookie.domain.trim_start_matches('.');
    let url = Url::parse(&format!("https://{host}/"))?;
    let mut cookie_line = format!(
        "{}={}; Domain={}; Path={}",
        cookie.name, cookie.value, cookie.domain, cookie.path
    );
    if cookie.secure {
        cookie_line.push_str("; Secure");
    }
    jar.add_cookie_str(&cookie_line, &url);
    Ok(())
}

pub fn dedupe_cookies(cookies: Vec<BrowserCookie>) -> Vec<BrowserCookie> {
    cookies
        .into_iter()
        .map(|cookie| {
            (
                (
                    cookie.domain.clone(),
                    cookie.path.clone(),
                    cookie.name.clone(),
                ),
                cookie,
            )
        })
        .collect::<HashMap<_, _>>()
        .into_values()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_cookie_header() {
        let cookies = parse_cookie_file("Cookie: sp_dc=abc; sp_key=def").unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "sp_dc");
    }

    #[test]
    fn parses_json_cookie_map() {
        let cookies = parse_cookie_file(r#"{"sp_dc":"abc","sp_key":"def"}"#).unwrap();
        assert_eq!(cookie_value(&cookies, "sp_key"), Some("def"));
    }

    #[test]
    fn parses_netscape_cookie_jar() {
        let input = ".spotify.com\tTRUE\t/\tTRUE\t1893456000\tsp_dc\tabc";
        let cookies = parse_cookie_file(input).unwrap();
        assert_eq!(cookies[0].domain, ".spotify.com");
        assert_eq!(cookies[0].name, "sp_dc");
    }
}
