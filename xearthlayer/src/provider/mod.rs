//! Satellite imagery provider abstraction
//!
//! This module provides traits and implementations for downloading satellite
//! imagery from various providers (Bing Maps, Google, NAIP, etc.).
//!
//! # Factory Pattern
//!
//! For centralized provider creation, use the [`ProviderFactory`]:
//!
//! ```ignore
//! use xearthlayer::provider::{ProviderFactory, ProviderConfig, ReqwestClient};
//!
//! let http_client = ReqwestClient::new()?;
//! let factory = ProviderFactory::new(http_client);
//! let (provider, name, max_zoom) = factory.create(&ProviderConfig::Bing)?;
//! ```

mod bing;
mod factory;
mod google;
mod http;
mod types;

pub use bing::BingMapsProvider;
pub use factory::{ProviderConfig, ProviderFactory};
pub use google::GoogleMapsProvider;
pub use http::{HttpClient, ReqwestClient};
pub use types::{Provider, ProviderError};

#[cfg(test)]
pub use http::tests::MockHttpClient;
