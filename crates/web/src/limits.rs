//! Fixed-size admission state: attacker-supplied keys cannot grow this limiter.
use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

struct Window {
    start: Instant,
    used: u32,
}
impl Window {
    fn take(&mut self, now: Instant, period: Duration, maximum: u32) -> bool {
        if now.duration_since(self.start) >= period {
            self.start = now;
            self.used = 0;
        }
        if self.used >= maximum {
            return false;
        }
        self.used += 1;
        true
    }
}

pub struct Limits {
    requests: Mutex<Window>,
    logins: Mutex<Window>,
    slots: Semaphore,
}
impl Limits {
    pub fn new(concurrency: usize) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Window {
                start: Instant::now(),
                used: 0,
            }),
            logins: Mutex::new(Window {
                start: Instant::now(),
                used: 0,
            }),
            slots: Semaphore::new(concurrency),
        })
    }
}

pub async fn admission(
    State(limits): State<Arc<Limits>>,
    request: Request,
    next: Next,
) -> Response {
    let now = Instant::now();
    let login = request.uri().path() == "/login";
    if !limits
        .requests
        .lock()
        .unwrap()
        .take(now, Duration::from_secs(1), 120)
        || (login
            && !limits
                .logins
                .lock()
                .unwrap()
                .take(now, Duration::from_secs(60), 20))
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            "Too many requests. Please retry later.",
        )
            .into_response();
    }
    let Ok(_permit) = limits.slots.try_acquire() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "Service busy. Please retry.",
        )
            .into_response();
    };
    match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await {
        Ok(response) => response,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "Request timed out.").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, middleware, routing::get};
    use tower::ServiceExt;
    #[tokio::test]
    async fn login_flood_is_rejected_before_handler_and_capacity_is_bounded() {
        let limits = Limits::new(1);
        let app = Router::new()
            .route("/login", get(|| async { "login" }))
            .route("/", get(|| async { "dashboard" }))
            .layer(middleware::from_fn_with_state(limits.clone(), admission));
        for _ in 0..20 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/login")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
        let _held = limits.slots.acquire().await.unwrap();
        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
