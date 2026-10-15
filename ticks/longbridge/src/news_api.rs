use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use longbridge::{
    Config, ContentContext,
    httpclient::{HttpClient, HttpClientConfig, Json, Method},
    oauth::OAuth,
};
use serde::Deserialize;
use time::{
    OffsetDateTime,
    format_description::well_known::{Rfc2822, Rfc3339},
};

use crate::{not_authorized, rate_limit::RateLimiter};
use tick_provider::{
    error::{Error, Result},
    news::{NewsApi, NewsArticle, NewsItem},
    preferences::{Language, Region},
};

const RSS_BASE_URL: &str = "https://longbridge.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

pub struct SdkNewsApi {
    content: ContentContext,
    signed_http: HttpClient,
    public_http: reqwest::Client,
    language: Language,
    limiter: Arc<RateLimiter>,
    articles: tokio::sync::Mutex<HashMap<String, NewsArticle>>,
}

impl SdkNewsApi {
    pub async fn new(
        config: Arc<Config>,
        oauth: OAuth,
        language: Language,
        region: Option<Region>,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self> {
        let token = oauth
            .access_token()
            .await
            .map_err(|_| not_authorized("authorization expired"))?;
        let region = if longbridge::DcRegion::from_credential(&token) == longbridge::DcRegion::Us {
            Region::Global
        } else {
            region.unwrap_or(Region::Global)
        };
        let base = match region {
            Region::Cn => "https://openapi.longbridge.cn",
            Region::Global => "https://openapi.longbridge.com",
        };
        let signed_http = HttpClient::new(HttpClientConfig::from_oauth(oauth).http_url(base))
            .header("accept-language", language.as_str());
        let public_http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("tickloop-ticks/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Error::network("cannot create news HTTP client"))?;
        Ok(Self {
            content: ContentContext::new(config),
            signed_http,
            public_http,
            language,
            limiter,
            articles: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    async fn general_news(&self) -> Result<Vec<NewsItem>> {
        let language = match self.language {
            Language::ZhCn => "zh-CN",
            Language::En => "en",
        };
        self.limiter.acquire().await;
        let response = self
            .public_http
            .get(format!("{RSS_BASE_URL}/{language}/news/feed"))
            .send()
            .await
            .map_err(|_| Error::network("news feed request failed"))?
            .error_for_status()
            .map_err(|error| {
                Error::network(format!(
                    "news feed failed: {}",
                    error
                        .status()
                        .map_or_else(|| "unknown status".into(), |status| status.to_string())
                ))
            })?;
        let xml = response
            .text()
            .await
            .map_err(|_| Error::network("cannot read news feed"))?;
        let feed: Feed =
            quick_xml::de::from_str(&xml).map_err(|_| Error::network("invalid news feed"))?;
        let mut items = Vec::with_capacity(feed.channel.items.len());
        for item in feed.channel.items {
            let url = item.link.trim().to_owned();
            let id = reqwest::Url::parse(&url)
                .ok()
                .and_then(|url| {
                    url.path_segments()?
                        .next_back()
                        .filter(|id| {
                            !id.is_empty() && id.chars().all(|character| character.is_ascii_digit())
                        })
                        .map(str::to_owned)
                })
                .ok_or_else(|| Error::network("news feed contains an invalid article ID"))?;
            let published_at = OffsetDateTime::parse(item.published_at.trim(), &Rfc2822)
                .or_else(|_| OffsetDateTime::parse(item.published_at.trim(), &Rfc3339))
                .ok();
            let (published_at, source) = if let Some(published_at) = published_at {
                (
                    published_at
                        .format(&Rfc3339)
                        .map_err(|_| Error::network("invalid news publication time"))?,
                    item.author.trim().to_owned(),
                )
            } else {
                // The official feed may omit pubDate; obtain its actual timestamp from the article
                // API. A feed entry the API no longer serves is dropped instead of failing the
                // whole list; only a lost authorization still stops it.
                let article = match self.article(&id).await {
                    Ok(article) => article,
                    Err(error) if error.code == tick_provider::exit_code::NOT_AUTHORIZED => {
                        return Err(error);
                    }
                    Err(_) => continue,
                };
                (
                    article.published_at,
                    if item.author.trim().is_empty() {
                        article.source
                    } else {
                        item.author.trim().to_owned()
                    },
                )
            };
            items.push(NewsItem {
                id,
                title: item.title.trim().to_owned(),
                source: if source.is_empty() {
                    "Longbridge".into()
                } else {
                    source
                },
                published_at,
                url,
            });
        }
        Ok(items)
    }
}

#[async_trait]
impl NewsApi for SdkNewsApi {
    async fn news(&self, symbol: Option<&str>) -> Result<Vec<NewsItem>> {
        let items = if let Some(symbol) = symbol {
            let items = self
                .limiter
                .execute(|| self.content.news(symbol))
                .await
                .map_err(crate::sdk::sdk_error)?;
            items
                .into_iter()
                .map(|item| {
                    Ok(NewsItem {
                        id: item.id,
                        title: item.title,
                        source: "Longbridge".into(),
                        url: item.url,
                        published_at: item
                            .published_at
                            .format(&Rfc3339)
                            .map_err(|_| Error::network("invalid news publication time"))?,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            self.general_news().await?
        };
        self.articles
            .lock()
            .await
            .retain(|id, _| items.iter().any(|item| &item.id == id));
        Ok(items)
    }

    async fn article(&self, id: &str) -> Result<NewsArticle> {
        if let Some(article) = self.articles.lock().await.get(id).cloned() {
            return Ok(article);
        }
        if id.is_empty() || !id.chars().all(|character| character.is_ascii_digit()) {
            return Err(Error::network("invalid article ID"));
        }
        let response = self
            .limiter
            .execute(|| {
                self.signed_http
                    .request(Method::GET, format!("/v1/content/news/{id}"))
                    .response::<Json<ArticleResponse>>()
                    .send()
            })
            .await
            .map_err(|error| crate::sdk::sdk_error(error.into()))?
            .0;
        let item = response.item;
        let timestamp = item
            .published_at
            .as_i64()
            .or_else(|| item.published_at.as_str()?.parse().ok())
            .ok_or_else(|| Error::network("missing or invalid article publication time"))?;
        let published_at = OffsetDateTime::from_unix_timestamp(timestamp)
            .ok()
            .and_then(|time| time.format(&Rfc3339).ok())
            .ok_or_else(|| Error::network("invalid article publication time"))?;
        let article = NewsArticle {
            id: id.to_owned(),
            title: item.title,
            source: if item.author.name.is_empty() {
                "Longbridge".into()
            } else {
                item.author.name
            },
            published_at,
            url: item.url,
            body: if item.body.is_empty() {
                item.description
            } else {
                item.body
            },
        };
        self.articles
            .lock()
            .await
            .insert(id.to_owned(), article.clone());
        Ok(article)
    }
}

#[derive(Deserialize)]
struct Feed {
    channel: FeedChannel,
}

#[derive(Deserialize)]
struct FeedChannel {
    #[serde(rename = "item", default)]
    items: Vec<FeedItem>,
}

#[derive(Deserialize)]
struct FeedItem {
    title: String,
    link: String,
    #[serde(default)]
    author: String,
    #[serde(rename = "pubDate", default)]
    published_at: String,
}

#[derive(Deserialize)]
struct ArticleResponse {
    item: ArticlePayload,
}

#[derive(Deserialize)]
struct ArticlePayload {
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    description: String,
    url: String,
    published_at: serde_json::Value,
    #[serde(default)]
    author: ArticleAuthor,
}

#[derive(Default, Deserialize)]
struct ArticleAuthor {
    #[serde(default)]
    name: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks_to_log_in;

    #[tokio::test]
    async fn a_lapsed_login_opens_no_news_api_and_asks_the_user_to_log_in_again() {
        let oauth = crate::auth::lapsing_oauth().await;
        let Err(error) = SdkNewsApi::new(
            Arc::new(Config::from_oauth(oauth.clone())),
            oauth,
            Language::default(),
            None,
            Arc::new(RateLimiter::default()),
        )
        .await
        else {
            panic!("a lapsed login opens no news API");
        };
        assert!(asks_to_log_in(&error), "{error}");
    }
}
