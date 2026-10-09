use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NewsItem {
    pub id: String,
    pub title: String,
    pub source: String,
    pub published_at: String,
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct NewsArticle {
    pub id: String,
    pub title: String,
    pub source: String,
    pub published_at: String,
    pub url: String,
    pub body: String,
}

#[async_trait]
pub trait NewsApi: Send + Sync {
    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>>;
    async fn article(&self, id: &str) -> Result<NewsArticle>;
}
