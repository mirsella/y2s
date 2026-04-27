pub mod auth;
pub mod hash;
pub mod playlist;
pub mod search;

use std::{collections::HashMap, sync::Arc, time::Duration};

use reqwest::{StatusCode, header};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    cookies::BrowserCookie,
    error::{AppError, Result, body_excerpt},
};

const GRAPHQL_ENDPOINT: &str = "https://api-partner.spotify.com/pathfinder/v1/query";
const ACCEPT_LANGUAGE: &str = "en";

#[derive(Clone)]
pub struct SpotifyClient {
    inner: Arc<SpotifyInner>,
}

struct SpotifyInner {
    http: reqwest::Client,
    auth: Mutex<auth::AuthState>,
    hashes: HashMap<String, String>,
    username: String,
}

impl SpotifyClient {
    pub async fn connect(cookies: Vec<BrowserCookie>) -> Result<Self> {
        let bootstrap = auth::bootstrap(cookies).await?;
        Ok(Self {
            inner: Arc::new(SpotifyInner {
                http: bootstrap.http,
                auth: Mutex::new(bootstrap.auth),
                hashes: bootstrap.hashes,
                username: bootstrap.username,
            }),
        })
    }

    pub(crate) fn http(&self) -> &reqwest::Client {
        &self.inner.http
    }

    pub(crate) fn username(&self) -> &str {
        &self.inner.username
    }

    pub(crate) fn has_graph_hash(&self, operation: &str) -> bool {
        self.inner.hashes.contains_key(operation)
    }

    pub(crate) async fn auth_headers(&self) -> Result<header::HeaderMap> {
        let auth = self.inner.auth.lock().await;
        auth_headers(&auth)
    }

    pub(crate) async fn graph_query(&self, operation: &str, variables: Value) -> Result<Value> {
        let hash = self.inner.hashes.get(operation).cloned().ok_or_else(|| {
            AppError::Spotify(format!(
                "missing persisted GraphQL hash for operation {operation}; Spotify web-player bundles may have changed"
            ))
        })?;

        let body = json!({
            "operationName": operation,
            "variables": variables,
            "extensions": {
                "persistedQuery": {
                    "version": 1,
                    "sha256Hash": hash,
                }
            }
        });

        let mut refreshed_access = false;
        let mut refreshed_client_token = false;
        let mut retried_after_rate_limit = false;

        loop {
            let headers = self.auth_headers().await?;
            let response = self
                .inner
                .http
                .post(GRAPHQL_ENDPOINT)
                .headers(headers)
                .json(&body)
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
                        "failed to parse Spotify GraphQL response for {operation} as JSON: {err}; body: {}",
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

            if status == StatusCode::BAD_REQUEST
                && !refreshed_client_token
                && text.to_ascii_lowercase().contains("client")
                && text.to_ascii_lowercase().contains("token")
            {
                self.refresh_client_token().await?;
                refreshed_client_token = true;
                continue;
            }

            return Err(AppError::HttpStatus {
                url: GRAPHQL_ENDPOINT.to_string(),
                status,
                body: body_excerpt(&text),
            });
        }
    }

    async fn refresh_access_token(&self) -> Result<()> {
        let mut auth = self.inner.auth.lock().await;
        let token = auth::fetch_access_token(&self.inner.http).await?;
        auth.access_token = token.access_token;
        auth.client_id = token.client_id;
        auth.access_expires_ms = token.expires_ms;
        Ok(())
    }

    async fn refresh_client_token(&self) -> Result<()> {
        let mut auth = self.inner.auth.lock().await;
        let token = auth::fetch_client_token(
            &self.inner.http,
            &auth.client_version,
            &auth.client_id,
            &auth.device_id,
        )
        .await?;
        auth.client_token = token;
        Ok(())
    }
}

fn auth_headers(auth: &auth::AuthState) -> Result<header::HeaderMap> {
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_str(&format!("Bearer {}", auth.access_token)).map_err(|err| {
            AppError::Spotify(format!("invalid Spotify access token header: {err}"))
        })?,
    );
    headers.insert(
        header::HeaderName::from_static("client-token"),
        header::HeaderValue::from_str(&auth.client_token).map_err(|err| {
            AppError::Spotify(format!("invalid Spotify client token header: {err}"))
        })?,
    );
    headers.insert(
        header::HeaderName::from_static("spotify-app-version"),
        header::HeaderValue::from_str(&auth.client_version).map_err(|err| {
            AppError::Spotify(format!("invalid Spotify app version header: {err}"))
        })?,
    );
    headers.insert(
        header::HeaderName::from_static("app-platform"),
        header::HeaderValue::from_static("WebPlayer"),
    );
    headers.insert(
        header::ACCEPT_LANGUAGE,
        header::HeaderValue::from_str(ACCEPT_LANGUAGE)
            .map_err(|err| AppError::Spotify(format!("invalid Accept-Language header: {err}")))?,
    );
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Ok(headers)
}
