use std::{sync::Arc, time::Duration};
use tick_longbridge::rate_limit::RateLimiter;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn non_rate_limit_error_containing_429_is_returned_without_retry() {
    let limiter = RateLimiter::new(10, Duration::ZERO);
    let start = Instant::now();
    let mut attempts = 0;
    let result: Result<(), _> = limiter
        .execute(|| {
            attempts += 1;
            async {
                Err(longbridge::Error::InvalidSecuritySymbol {
                    symbol: "429.HK".into(),
                })
            }
        })
        .await;
    assert!(matches!(
        result,
        Err(longbridge::Error::InvalidSecuritySymbol { .. })
    ));
    assert_eq!(attempts, 1);
    assert_eq!(start.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_obey_window_and_minimum_spacing() {
    let limiter = Arc::new(RateLimiter::new(2, Duration::from_millis(20)));
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let limiter = limiter.clone();
        tasks.push(tokio::spawn(async move {
            limiter.acquire().await;
            Instant::now() - start
        }));
    }
    let mut times = Vec::new();
    for task in tasks {
        times.push(task.await.unwrap());
    }
    times.sort();
    assert_eq!(
        times,
        [
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_secs(1),
            Duration::from_millis(1020)
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn server_rejection_pauses_every_waiter_beyond_the_minimum_gap() {
    let limiter = Arc::new(RateLimiter::new(10, Duration::from_millis(20)));
    let start = Instant::now();
    let task_limiter = limiter.clone();
    let (rejected, rejection) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut rejected = Some(rejected);
        task_limiter
            .execute(|| {
                let first = rejected.take();
                async move {
                    if let Some(rejected) = first {
                        let _ = rejected.send(());
                        Err(longbridge::httpclient::HttpClientError::OpenApi {
                            code: 429_002,
                            message: "API request is limited".into(),
                            trace_id: String::new(),
                        })
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap();
    });
    rejection.await.unwrap();
    limiter.acquire().await;
    assert!(start.elapsed() >= Duration::from_secs(1));
    task.await.unwrap();
    assert!(start.elapsed() >= Duration::from_millis(1020));
}

#[tokio::test(start_paused = true)]
async fn http_retry_after_header_controls_the_shared_pause_without_reading_error_text() {
    use longbridge::httpclient::HttpClientError;
    use reqwest::{
        StatusCode,
        header::{HeaderMap, RETRY_AFTER},
    };

    let limiter = RateLimiter::new(10, Duration::ZERO);
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, "3".parse().unwrap());
    let mut error = Some(longbridge::Error::HttpClient(
        HttpClientError::UnexpectedHttpResponse {
            status: StatusCode::TOO_MANY_REQUESTS,
            trace_id: String::new(),
            headers: Box::new(headers),
            body: "retry after: 0.1s".into(),
        },
    ));
    let start = Instant::now();
    limiter
        .execute(|| {
            let error = error.take();
            async move { error.map_or(Ok(()), Err) }
        })
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn http_retry_after_date_delays_until_the_server_time() {
    use longbridge::httpclient::HttpClientError;
    use reqwest::{
        StatusCode,
        header::{HeaderMap, RETRY_AFTER},
    };
    use time::{OffsetDateTime, format_description::well_known::Rfc2822};

    let limiter = RateLimiter::new(10, Duration::ZERO);
    let future = OffsetDateTime::now_utc() + time::Duration::days(1);
    let mut headers = HeaderMap::new();
    headers.insert(
        RETRY_AFTER,
        future
            .format(&Rfc2822)
            .unwrap()
            .replace("+0000", "GMT")
            .parse()
            .unwrap(),
    );
    let mut error = Some(HttpClientError::UnexpectedHttpResponse {
        status: StatusCode::TOO_MANY_REQUESTS,
        trace_id: String::new(),
        headers: Box::new(headers),
        body: String::new(),
    });
    let start = Instant::now();
    limiter
        .execute(|| {
            let error = error.take();
            async move { error.map_or(Ok(()), Err) }
        })
        .await
        .unwrap();
    assert!(start.elapsed() > Duration::from_secs(23 * 60 * 60));
    assert!(start.elapsed() <= Duration::from_secs(24 * 60 * 60));
}

#[tokio::test(start_paused = true)]
async fn only_known_http_and_websocket_rate_limit_codes_retry() {
    use longbridge::{
        Error,
        httpclient::HttpClientError,
        wsclient::{WsClientError, WsResponseErrorDetail},
    };
    use reqwest::{StatusCode, header::HeaderMap};

    let api_error = |code| {
        Error::HttpClient(HttpClientError::OpenApi {
            code,
            message: "request mentioning 429 rate limit".into(),
            trace_id: String::new(),
        })
    };
    let errors = [
        (api_error(429_001), true),
        (api_error(429_002), true),
        (api_error(401_003), false),
        (api_error(429_003), false),
        (
            Error::HttpClient(HttpClientError::BadStatus(StatusCode::TOO_MANY_REQUESTS)),
            true,
        ),
        (
            Error::HttpClient(HttpClientError::UnexpectedHttpResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                trace_id: "429002".into(),
                headers: Box::new(HeaderMap::new()),
                body: "rate limit for symbol 429.HK".into(),
            }),
            false,
        ),
        (
            Error::WsClient(WsClientError::ResponseError {
                status: 3,
                detail: Some(WsResponseErrorDetail {
                    code: 301_606,
                    msg: String::new(),
                }),
            }),
            true,
        ),
        (
            Error::WsClient(WsClientError::ResponseError {
                status: 3,
                detail: Some(WsResponseErrorDetail {
                    code: 301_600,
                    msg: "429 rate limit".into(),
                }),
            }),
            false,
        ),
    ];
    for (error, should_retry) in errors {
        let limiter = RateLimiter::new(10, Duration::ZERO);
        let start = Instant::now();
        let mut error = Some(error);
        let mut attempts = 0;
        let result = limiter
            .execute(|| {
                attempts += 1;
                let error = error.take();
                async move { error.map_or(Ok(()), Err) }
            })
            .await;
        assert_eq!(result.is_ok(), should_retry);
        assert_eq!(attempts, if should_retry { 2 } else { 1 });
        assert_eq!(
            start.elapsed(),
            if should_retry {
                Duration::from_secs(1)
            } else {
                Duration::ZERO
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn unavailable_retry_header_uses_exponential_backoff_and_stops_after_five_retries() {
    use longbridge::httpclient::HttpClientError;

    let limiter = RateLimiter::new(10, Duration::ZERO);
    let start = Instant::now();
    let mut attempts = Vec::new();
    let result: Result<(), _> = limiter
        .execute(|| {
            attempts.push(start.elapsed());
            async {
                Err(HttpClientError::OpenApi {
                    code: 429_002,
                    message: "retry after: 0.001s".into(),
                    trace_id: String::new(),
                })
            }
        })
        .await;
    assert!(matches!(
        result,
        Err(HttpClientError::OpenApi { code: 429_002, .. })
    ));
    assert_eq!(attempts, [0, 1, 3, 7, 15, 31].map(Duration::from_secs));
}
