use thiserror::Error;

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("YouTube playlist error: {0}")]
    Youtube(String),

    #[error("Spotify web API error: {0}")]
    Spotify(String),

    #[error("cookie error: {0}")]
    Cookie(String),

    #[error("HTTP {status} from {url}: {body}")]
    HttpStatus {
        url: String,
        status: reqwest::StatusCode,
        body: String,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Url(#[from] url::ParseError),

    #[error("dialog error: {0}")]
    Dialog(#[from] dialoguer::Error),
}

pub fn body_excerpt(body: &str) -> String {
    const MAX: usize = 600;
    let body = body.trim();
    if body.chars().count() <= MAX {
        body.to_string()
    } else {
        let excerpt = body.chars().take(MAX).collect::<String>();
        format!("{excerpt}...")
    }
}
