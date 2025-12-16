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

mod apple;
mod arcgis;
mod bing;
mod factory;
mod go2;
mod google;
mod http;
mod mapbox;
mod types;
mod usgs;

pub use apple::{AppleMapsProvider, AsyncAppleMapsProvider};
pub use arcgis::{ArcGisProvider, AsyncArcGisProvider};
pub use bing::{AsyncBingMapsProvider, BingMapsProvider};
pub use factory::{AsyncProviderFactory, AsyncProviderType, ProviderConfig, ProviderFactory};
pub use go2::{AsyncGo2Provider, Go2Provider};
pub use google::{AsyncGoogleMapsProvider, GoogleMapsProvider};
pub use http::{AsyncHttpClient, AsyncReqwestClient, HttpClient, ReqwestClient};
pub use mapbox::{AsyncMapBoxProvider, MapBoxProvider};
pub use types::{AsyncProvider, Provider, ProviderError};
pub use usgs::{AsyncUsgsProvider, UsgsProvider};

#[cfg(test)]
pub use http::tests::{MockAsyncHttpClient, MockHttpClient};
