//! GitHub App authentication and repository automation.
pub mod accounts;
pub mod client;
pub mod repository;
pub use accounts::AccountDirectory;
pub use client::{Account, AppConfig, GitHub};
