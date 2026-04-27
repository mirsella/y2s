use std::{
    collections::HashMap,
    env,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use regex::Regex;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use sha1::Sha1;
use uuid::Uuid;

use crate::{
    cookies::{BrowserCookie, cookie_jar, cookie_value},
    error::{AppError, Result, body_excerpt},
    spotify::hash,
};

const SPOTIFY_OPEN: &str = "https://open.spotify.com";
const SPOTIFY_TOKEN: &str = "https://open.spotify.com/api/token";
const CLIENT_TOKEN: &str = "https://clienttoken.spotify.com/v1/clienttoken";
const PROFILE: &str = "https://www.spotify.com/api/account-settings/v1/profile";
const SECRET_SOURCE: &str =
    "https://code.thetadev.de/ThetaDev/spotify-secrets/raw/branch/main/secrets/secretDict.json";
const EMBEDDED_TOTP_VERSION: u64 = 61;
const EMBEDDED_TOTP_SECRET: &[u8] = &[
    44, 55, 47, 42, 70, 40, 34, 114, 76, 74, 50, 111, 120, 97, 75, 76, 94, 102, 43, 69, 49, 120,
    118, 80, 64, 78,
];

type HmacSha1 = Hmac<Sha1>;

#[derive(Debug, Clone)]
pub struct AuthState {
    pub client_version: String,
    pub access_token: String,
    pub client_id: String,
    pub device_id: String,
    pub access_expires_ms: Option<i64>,
    pub client_token: String,
}

#[derive(Debug)]
pub struct Bootstrap {
    pub http: reqwest::Client,
    pub auth: AuthState,
    pub hashes: HashMap<String, String>,
    pub username: String,
}

#[derive(Debug)]
pub struct AccessToken {
    pub access_token: String,
    pub client_id: String,
    pub expires_ms: Option<i64>,
}

const ACCEPT_LANGUAGE: &str = "en";

pub async fn bootstrap(cookies: Vec<BrowserCookie>) -> Result<Bootstrap> {
    let jar = cookie_jar(&cookies)?;
    let mut headers = default_headers();
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("text/html,application/xhtml+xml"),
    );

    let http = reqwest::Client::builder()
        .default_headers(headers)
        .cookie_provider(jar)
        .build()?;

    let response = http.get(SPOTIFY_OPEN).send().await?;
    let status = response.status();
    let html = response.text().await?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            url: SPOTIFY_OPEN.to_string(),
            status,
            body: body_excerpt(&html),
        });
    }

    let app_config = parse_app_config(&html)?;
    let client_version = app_config.client_version.ok_or_else(|| {
        AppError::Spotify("Spotify appServerConfig did not include clientVersion".to_string())
    })?;

    let hashes = hash::discover_hashes(&http, &html).await?;
    let access = fetch_access_token(&http).await?;
    let device_id = cookie_value(&cookies, "sp_t")
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let client_token =
        fetch_client_token(&http, &client_version, &access.client_id, &device_id).await?;
    let username = fetch_username(&http).await?;

    if cookie_value(&cookies, "sp_dc").is_none() {
        return Err(AppError::Spotify(
            "selected cookie source is missing sp_dc after validation; cannot authenticate Spotify web session".to_string(),
        ));
    }

    Ok(Bootstrap {
        http,
        auth: AuthState {
            client_version,
            access_token: access.access_token,
            client_id: access.client_id,
            device_id,
            access_expires_ms: access.expires_ms,
            client_token,
        },
        hashes,
        username,
    })
}

pub async fn fetch_access_token(http: &reqwest::Client) -> Result<AccessToken> {
    let secret = load_totp_secret(http).await?;
    let totp = generate_totp(&secret.secret, current_unix_seconds())?;

    let mut request = http
        .get(SPOTIFY_TOKEN)
        .query(&[
            ("reason", "init".to_string()),
            ("productType", "web-player".to_string()),
            ("totp", totp.clone()),
            ("totpServer", totp),
            ("totpVer", secret.version.to_string()),
        ])
        .header(header::ACCEPT, "application/json");

    if let Ok(ts) = SystemTime::now().duration_since(UNIX_EPOCH) {
        request = request.query(&[("ts", ts.as_millis().to_string())]);
    }

    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            url: SPOTIFY_TOKEN.to_string(),
            status,
            body: body_excerpt(&text),
        });
    }

    let json: Value = serde_json::from_str(&text).map_err(|err| {
        AppError::Spotify(format!(
            "failed to parse Spotify /api/token response as JSON: {err}; body: {}",
            body_excerpt(&text)
        ))
    })?;
    let access_token = json
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::Spotify("Spotify /api/token response missing accessToken".to_string())
        })?
        .to_string();
    let client_id = json
        .get("clientId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AppError::Spotify("Spotify /api/token response missing clientId".to_string())
        })?
        .to_string();
    let expires_ms = json
        .get("accessTokenExpirationTimestampMs")
        .and_then(Value::as_i64);

    Ok(AccessToken {
        access_token,
        client_id,
        expires_ms,
    })
}

pub async fn fetch_client_token(
    http: &reqwest::Client,
    client_version: &str,
    client_id: &str,
    device_id: &str,
) -> Result<String> {
    let body = json!({
        "client_data": {
            "client_version": client_version,
            "client_id": client_id,
            "js_sdk_data": {
                "device_brand": "unknown",
                "device_model": "unknown",
                "os": "windows",
                "os_version": "NT 10.0",
                "device_id": device_id,
                "device_type": "computer"
            }
        }
    });

    let response = http
        .post(CLIENT_TOKEN)
        .header(header::ACCEPT, "application/json")
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            url: CLIENT_TOKEN.to_string(),
            status,
            body: body_excerpt(&text),
        });
    }

    let json: Value = serde_json::from_str(&text).map_err(|err| {
        AppError::Spotify(format!(
            "failed to parse Spotify clienttoken response as JSON: {err}; body: {}",
            body_excerpt(&text)
        ))
    })?;
    json.pointer("/granted_token/token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            AppError::Spotify("clienttoken response missing granted_token.token".to_string())
        })
}

async fn fetch_username(http: &reqwest::Client) -> Result<String> {
    let response = http
        .get(PROFILE)
        .header(header::ACCEPT, "application/json")
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            url: PROFILE.to_string(),
            status,
            body: body_excerpt(&text),
        });
    }

    let json: Value = serde_json::from_str(&text).map_err(|err| {
        AppError::Spotify(format!(
            "failed to parse Spotify profile response as JSON: {err}; body: {}",
            body_excerpt(&text)
        ))
    })?;
    json.get("username")
        .and_then(Value::as_str)
        .or_else(|| json.pointer("/profile/username").and_then(Value::as_str))
        .map(str::to_string)
        .ok_or_else(|| AppError::Spotify("Spotify profile response missing username".to_string()))
}

fn default_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123 Safari/537.36",
        ),
    );
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_static(ACCEPT_LANGUAGE),
    );
    headers.insert(header::ORIGIN, HeaderValue::from_static(SPOTIFY_OPEN));
    headers.insert(
        header::REFERER,
        HeaderValue::from_static("https://open.spotify.com/"),
    );
    headers
}

#[derive(Debug, Default)]
struct AppConfig {
    client_version: Option<String>,
}

fn parse_app_config(html: &str) -> Result<AppConfig> {
    let re =
        Regex::new(r#"(?s)<script[^>]+id=["']appServerConfig["'][^>]*>(?P<body>.*?)</script>"#)
            .expect("appServerConfig regex is valid");
    let Some(captures) = re.captures(html) else {
        return Err(AppError::Spotify(
            "Spotify landing page did not contain appServerConfig".to_string(),
        ));
    };

    let encoded = html_escape::decode_html_entities(&captures["body"]);
    let decoded = STANDARD.decode(encoded.trim()).map_err(|err| {
        AppError::Spotify(format!("failed to base64-decode appServerConfig: {err}"))
    })?;
    let value: Value = serde_json::from_slice(&decoded)?;
    Ok(AppConfig {
        client_version: find_string_key(&value, "clientVersion").map(str::to_string),
    })
}

#[derive(Debug, Clone)]
struct TotpSecret {
    version: u64,
    secret: Vec<u8>,
}

async fn load_totp_secret(http: &reqwest::Client) -> Result<TotpSecret> {
    if let Ok(secret) = env::var("Y2S_SPOTIFY_TOTP_SECRET") {
        let version = env::var("Y2S_SPOTIFY_TOTP_VERSION")
            .ok()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                AppError::Spotify(
                    "Y2S_SPOTIFY_TOTP_SECRET is set, but Y2S_SPOTIFY_TOTP_VERSION is missing or invalid".to_string(),
                )
            })?;
        return Ok(TotpSecret {
            version,
            secret: secret.into_bytes(),
        });
    }

    match fetch_remote_totp_secret(http).await {
        Ok(secret) => Ok(secret),
        Err(err) => {
            eprintln!(
                "WARN: failed to fetch latest Spotify TOTP secret; using embedded fallback: {err}"
            );
            Ok(embedded_totp_secret())
        }
    }
}

async fn fetch_remote_totp_secret(http: &reqwest::Client) -> Result<TotpSecret> {
    let response = http.get(SECRET_SOURCE).send().await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(AppError::HttpStatus {
            url: SECRET_SOURCE.to_string(),
            status,
            body: body_excerpt(&text),
        });
    }

    let secrets: HashMap<String, Vec<u8>> = serde_json::from_str(&text)?;
    let (version, secret) = secrets
        .into_iter()
        .filter_map(|(version, secret)| {
            version.parse::<u64>().ok().map(|version| (version, secret))
        })
        .max_by_key(|(version, _)| *version)
        .ok_or_else(|| {
            AppError::Spotify("spotify-secrets source returned an empty secret list".to_string())
        })?;
    Ok(TotpSecret {
        version,
        secret: transform_totp_secret(&secret),
    })
}

fn embedded_totp_secret() -> TotpSecret {
    TotpSecret {
        version: EMBEDDED_TOTP_VERSION,
        secret: transform_totp_secret(EMBEDDED_TOTP_SECRET),
    }
}

fn transform_totp_secret(secret: &[u8]) -> Vec<u8> {
    secret
        .iter()
        .enumerate()
        .map(|(index, byte)| (byte ^ ((index % 33) as u8 + 9)).to_string())
        .collect::<String>()
        .into_bytes()
}

fn generate_totp(secret: &[u8], timestamp_seconds: u64) -> Result<String> {
    let counter = timestamp_seconds / 30;
    let mut mac = HmacSha1::new_from_slice(secret)
        .map_err(|err| AppError::Spotify(format!("invalid TOTP secret: {err}")))?;
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let binary = ((u32::from(digest[offset]) & 0x7f) << 24)
        | (u32::from(digest[offset + 1]) << 16)
        | (u32::from(digest[offset + 2]) << 8)
        | u32::from(digest[offset + 3]);
    Ok(format!("{:06}", binary % 1_000_000))
}

fn current_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time is after unix epoch")
        .as_secs()
}

fn find_string_key<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key).and_then(Value::as_str) {
                return Some(found);
            }
            map.values().find_map(|child| find_string_key(child, key))
        }
        Value::Array(items) => items.iter().find_map(|child| find_string_key(child, key)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_rfc_totp() {
        assert_eq!(
            generate_totp(b"12345678901234567890", 59).unwrap(),
            "287082"
        );
    }
}
