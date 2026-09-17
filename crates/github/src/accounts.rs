//! Public account lookup for local administration without GitHub App credentials.
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use grading_core::config::identifier;
use reqwest::Client;

use crate::{Account, client::bounded_bytes};

pub struct AccountDirectory {
    http: Client,
    api_origin: String,
}

impl AccountDirectory {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: Client::builder()
                .user_agent("srg-grading/0.1")
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            api_origin: "https://api.github.com".into(),
        })
    }

    pub async fn resolve(&self, username: &str) -> Result<Account> {
        ensure!(identifier(username), "invalid GitHub username");
        let account = self
            .get(&format!("/users/{username}"))
            .await
            .with_context(|| format!("could not resolve GitHub username @{username}"))?;
        ensure!(
            account.login.eq_ignore_ascii_case(username),
            "GitHub returned another username; use the account's current handle"
        );
        Ok(account)
    }

    pub async fn account(&self, id: i64) -> Result<Account> {
        ensure!(id > 0, "invalid stored GitHub account ID");
        let account = self.get(&format!("/user/{id}")).await?;
        ensure!(account.id == id, "GitHub account identity mismatch");
        Ok(account)
    }

    async fn get(&self, path: &str) -> Result<Account> {
        let response = self
            .http
            .get(format!("{}{path}", self.api_origin))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let account: Account = serde_json::from_slice(&bounded_bytes(response, 65536).await?)?;
        ensure!(
            account.id > 0 && account.kind == "User" && identifier(&account.login),
            "GitHub account must be an individual user"
        );
        Ok(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, http::StatusCode, routing::get};
    use serde_json::json;

    #[tokio::test]
    async fn handles_resolve_to_verified_users_and_listing_tracks_renames() {
        let app = Router::new()
            .route(
                "/users/current-handle",
                get(|| async { Json(json!({"id":42,"login":"current-handle","type":"User"})) }),
            )
            .route(
                "/users/123",
                get(|| async { Json(json!({"id":77,"login":"123","type":"User"})) }),
            )
            .route(
                "/users/organization",
                get(|| async {
                    Json(json!({"id":43,"login":"organization","type":"Organization"}))
                }),
            )
            .route(
                "/users/wrong-user",
                get(|| async { Json(json!({"id":44,"login":"other-user","type":"User"})) }),
            )
            .route(
                "/users/old-handle",
                get(|| async {
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [("location", "/users/current-handle")],
                    )
                }),
            )
            .route(
                "/users/unavailable",
                get(|| async { StatusCode::TOO_MANY_REQUESTS }),
            )
            .route(
                "/user/42",
                get(|| async { Json(json!({"id":42,"login":"renamed-handle","type":"User"})) }),
            )
            .route(
                "/user/43",
                get(|| async { Json(json!({"id":99,"login":"wrong-id","type":"User"})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut directory = AccountDirectory::new().unwrap();
        directory.api_origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert_eq!(directory.resolve("current-handle").await.unwrap().id, 42);
        assert_eq!(directory.resolve("123").await.unwrap().id, 77);
        assert_eq!(directory.account(42).await.unwrap().login, "renamed-handle");
        for handle in [
            "missing",
            "organization",
            "wrong-user",
            "old-handle",
            "unavailable",
            "../user/42",
        ] {
            assert!(
                directory.resolve(handle).await.is_err(),
                "accepted {handle}"
            );
        }
        assert!(directory.account(43).await.is_err());
        server.abort();
    }
}
