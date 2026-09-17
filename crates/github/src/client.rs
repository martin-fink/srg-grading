//! Bounded GitHub API transport, installation tokens, and PKCE login exchange.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use grading_core::{config::identifier, security::pkce};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::{Client, Method, StatusCode, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub app_id: String,
    pub installation_id: i64,
    pub client_id: String,
    pub client_secret: String,
    pub private_key_file: std::path::PathBuf,
    pub callback_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: i64,
    pub login: String,
    #[serde(rename = "type", default)]
    pub kind: String,
}

#[derive(Clone)]
pub struct GitHub {
    pub(crate) http: Client,
    config: Arc<AppConfig>,
    key: Arc<EncodingKey>,
    cached_token: Arc<Mutex<Option<InstallationToken>>>,
    api_origin: String,
    rate_limit_until: Arc<Mutex<Option<DateTime<Utc>>>>,
}

struct InstallationToken {
    value: String,
    expires_at: DateTime<Utc>,
}

impl GitHub {
    pub async fn from_file(path: &Path) -> Result<Self> {
        let config: AppConfig = serde_json::from_slice(&tokio::fs::read(path).await?)?;
        Self::new(config).await
    }

    pub async fn new(config: AppConfig) -> Result<Self> {
        let callback = Url::parse(&config.callback_url)?;
        ensure!(
            callback.scheme() == "https"
                && callback.query().is_none()
                && callback.fragment().is_none(),
            "callback must be a fixed HTTPS URL"
        );
        ensure!(
            config.installation_id > 0 && !config.client_secret.is_empty(),
            "invalid GitHub App configuration"
        );
        let key = EncodingKey::from_rsa_pem(&tokio::fs::read(&config.private_key_file).await?)?;
        let http = Client::builder()
            .user_agent("srg-grading/0.1")
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            config: Arc::new(config),
            key: Arc::new(key),
            cached_token: Arc::new(Mutex::new(None)),
            api_origin: "https://api.github.com".into(),
            rate_limit_until: Arc::new(Mutex::new(None)),
        })
    }

    pub fn authorize_url(&self, state: &str, verifier: &str) -> String {
        let mut url = Url::parse("https://github.com/login/oauth/authorize").expect("static URL");
        url.query_pairs_mut().extend_pairs([
            ("client_id", self.config.client_id.as_str()),
            ("redirect_uri", self.config.callback_url.as_str()),
            ("state", state),
            ("code_challenge", pkce(verifier).as_str()),
            ("code_challenge_method", "S256"),
            ("allow_signup", "false"),
        ]);
        url.to_string()
    }

    pub async fn login(&self, code: &str, verifier: &str) -> Result<Account> {
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
        }
        let response = self
            .http
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("client_secret", self.config.client_secret.as_str()),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", self.config.callback_url.as_str()),
            ])
            .send()
            .await?;
        let token: Token = decode(response, 65536).await?;
        let response = self
            .http
            .get("https://api.github.com/user")
            .bearer_auth(&token.access_token)
            .send()
            .await?;
        let account: Account = decode(response, 65536).await?;
        ensure!(
            account.id > 0 && account.kind == "User",
            "login is not an individual GitHub account"
        );
        Ok(account)
    }

    async fn installation_token(&self) -> Result<String> {
        let mut cached = self.cached_token.lock().await;
        if let Some(token) = cached.as_ref()
            && token.expires_at > Utc::now() + chrono::Duration::minutes(2)
        {
            return Ok(token.value.clone());
        }
        #[derive(Serialize)]
        struct Claims<'a> {
            iat: i64,
            exp: i64,
            iss: &'a str,
        }
        let now = Utc::now().timestamp();
        let jwt = encode(
            &Header::new(Algorithm::RS256),
            &Claims {
                iat: now - 60,
                exp: now + 540,
                iss: &self.config.app_id,
            },
            &self.key,
        )?;
        #[derive(Deserialize)]
        struct Token {
            token: String,
            expires_at: DateTime<Utc>,
        }
        let response = self
            .http
            .post(format!(
                "https://api.github.com/app/installations/{}/access_tokens",
                self.config.installation_id
            ))
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .json(&json!({}))
            .send()
            .await?;
        let token: Token = decode(response, 65536).await?;
        *cached = Some(InstallationToken {
            value: token.token.clone(),
            expires_at: token.expires_at,
        });
        Ok(token.token)
    }

    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let response = self.response(method, path, body).await?;
        decode(response, 16 * 1024 * 1024).await
    }

    pub async fn optional<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        let response = self.response(Method::GET, path, None).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(decode(response, 16 * 1024 * 1024).await?))
    }

    pub async fn empty(&self, method: Method, path: &str, body: Option<Value>) -> Result<()> {
        let response = self.response(method, path, body).await?;
        ensure!(
            response.status().is_success(),
            "GitHub API returned HTTP {}",
            response.status()
        );
        Ok(())
    }

    async fn response(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response> {
        if self.retry_at().await.is_some_and(|at| at > Utc::now()) {
            anyhow::bail!("GitHub rate limit is active");
        }
        ensure!(
            path.starts_with('/') && !path.contains("..") && !path.contains('#'),
            "invalid GitHub API path"
        );
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.api_origin))
            .bearer_auth(self.installation_token().await?)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("GitHub transport failed")?;
        if response.status() == StatusCode::TOO_MANY_REQUESTS
            || (response.status() == StatusCode::FORBIDDEN
                && response
                    .headers()
                    .get("x-ratelimit-remaining")
                    .is_some_and(|v| v == "0"))
        {
            let retry = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(60)
                .clamp(1, 86400);
            let reset = response
                .headers()
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<i64>().ok())
                .and_then(|t| DateTime::from_timestamp(t, 0));
            *self.rate_limit_until.lock().await = Some(
                reset
                    .unwrap_or_else(|| Utc::now() + chrono::Duration::seconds(retry))
                    .max(Utc::now() + chrono::Duration::seconds(retry)),
            );
        }
        if response.status() == StatusCode::UNAUTHORIZED {
            *self.cached_token.lock().await = None;
        }
        Ok(response)
    }

    pub async fn retry_at(&self) -> Option<DateTime<Utc>> {
        *self.rate_limit_until.lock().await
    }

    #[cfg(test)]
    pub(crate) fn fixture(api_origin: String) -> Self {
        Self {
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            config: Arc::new(AppConfig {
                app_id: "1".into(),
                installation_id: 1,
                client_id: String::new(),
                client_secret: String::new(),
                private_key_file: "unused".into(),
                callback_url: "https://example.invalid/auth/callback".into(),
            }),
            key: Arc::new(EncodingKey::from_secret(b"fixture")),
            cached_token: Arc::new(Mutex::new(Some(InstallationToken {
                value: "fixture".into(),
                expires_at: Utc::now() + chrono::Duration::hours(1),
            }))),
            api_origin,
            rate_limit_until: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn resolve(&self, username: &str) -> Result<Account> {
        ensure!(identifier(username), "invalid GitHub username");
        let account: Account = self
            .request(Method::GET, &format!("/users/{username}"), None)
            .await?;
        ensure!(
            account.kind == "User" && account.id > 0,
            "roster must identify a user"
        );
        Ok(account)
    }

    pub async fn account(&self, id: i64) -> Result<Account> {
        ensure!(id > 0, "invalid account ID");
        self.request(Method::GET, &format!("/user/{id}"), None)
            .await
    }
}

pub async fn bounded_bytes(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    ensure!(response.status().is_success(), "HTTP {}", response.status());
    ensure!(
        response.content_length().is_none_or(|n| n <= limit as u64),
        "response too large"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(bytes.len() + chunk.len() <= limit, "response too large");
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn decode<T: DeserializeOwned>(response: reqwest::Response, limit: usize) -> Result<T> {
    Ok(serde_json::from_slice(
        &bounded_bytes(response, limit).await?,
    )?)
}
