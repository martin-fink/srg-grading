//! GitHub App authentication and repository automation.
pub mod client;
pub mod repository;
pub use client::{Account, AppConfig, GitHub};
