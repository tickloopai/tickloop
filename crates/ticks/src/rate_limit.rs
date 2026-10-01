//! Adapted sliding-window pacing from Longbridge Terminal; see THIRD_PARTY_NOTICES.md.
use longbridge::httpclient::HttpClientError;
use reqwest::{StatusCode, header::RETRY_AFTER};
use std::{collections::VecDeque, sync::Mutex, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc2822};
use tokio::time::{Instant, sleep_until};

const WINDOW: Duration = Duration::from_secs(1);
const MAX_RETRIES: u32 = 5;
const RATE_LIMIT_CODES: [i64; 3] = [429_001, 429_002, 301_606];

pub trait Retryable {
    fn is_rate_limited(&self) -> bool;

    fn retry_after(&self) -> Option<Duration> {
        None
    }
}

impl Retryable for HttpClientError {
    fn is_rate_limited(&self) -> bool {
        match self {
            Self::OpenApi { code, .. } => RATE_LIMIT_CODES.contains(&i64::from(*code)),
            Self::BadStatus(status) | Self::UnexpectedHttpResponse { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::Http(error) => error.0.status() == Some(StatusCode::TOO_MANY_REQUESTS),
            _ => false,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        let Self::UnexpectedHttpResponse { headers, .. } = self else {
            return None;
        };
        let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        let deadline = OffsetDateTime::parse(value, &Rfc2822).ok()?;
        Some(
            (deadline - OffsetDateTime::now_utc())
                .try_into()
                .unwrap_or(Duration::ZERO),
        )
    }
}

impl Retryable for longbridge::Error {
    fn is_rate_limited(&self) -> bool {
        match self {
            Self::HttpClient(error) => error.is_rate_limited(),
            _ => self
                .openapi_error_code()
                .is_some_and(|code| RATE_LIMIT_CODES.contains(&code)),
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::HttpClient(error) => error.retry_after(),
            _ => None,
        }
    }
}

pub struct RateLimiter {
    maximum: usize,
    minimum_gap: Duration,
    grants: tokio::sync::Mutex<VecDeque<Instant>>,
    paused_until: Mutex<Option<Instant>>,
}

impl RateLimiter {
    pub fn new(maximum: usize, minimum_gap: Duration) -> Self {
        assert!(maximum > 0, "rate limit must allow at least one request");
        Self {
            maximum,
            minimum_gap,
            grants: tokio::sync::Mutex::new(VecDeque::new()),
            paused_until: Mutex::new(None),
        }
    }

    pub async fn execute<F, Fut, T, E>(&self, mut request: F) -> std::result::Result<T, E>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: Retryable,
    {
        let mut retries = 0;
        loop {
            self.acquire().await;
            match request().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.is_rate_limited() || retries == MAX_RETRIES {
                        return Err(error);
                    }
                    let delay = error
                        .retry_after()
                        .unwrap_or(Duration::from_secs(1 << retries));
                    let Some(until) = Instant::now().checked_add(delay) else {
                        return Err(error);
                    };
                    let mut pause = self.paused_until.lock().unwrap();
                    *pause = Some(pause.unwrap_or(until).max(until));
                    retries += 1;
                }
            }
        }
    }

    pub async fn acquire(&self) {
        let mut grants = self.grants.lock().await;
        loop {
            let now = Instant::now();
            let last = grants.back().copied();
            while grants
                .front()
                .is_some_and(|oldest| now.duration_since(*oldest) >= WINDOW)
            {
                grants.pop_front();
            }
            let mut earliest = self.paused_until.lock().unwrap().unwrap_or(now).max(now);
            if let Some(last) = last {
                earliest = earliest.max(last + self.minimum_gap);
            }
            if grants.len() >= self.maximum {
                earliest = earliest.max(grants[0] + WINDOW);
            }
            if earliest <= now {
                grants.push_back(now);
                return;
            }
            sleep_until(earliest).await;
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(10, Duration::from_millis(100))
    }
}
