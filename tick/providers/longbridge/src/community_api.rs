use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use longbridge::{
    Config, ContentContext,
    content::{CreateReplyOptions, TopicReply},
    httpclient::HttpClientError,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::rate_limit::RateLimiter;
use tick_provider::{
    community::{
        CommunityApi, MemberPost, MemberProfile, PostKind, PostedReply, reply_outcome_unknown,
    },
    error::{Error, Result},
};

/// The public endpoints longbridge.com reads profiles and their activity feeds from; the
/// OpenAPI has no member feed, so reading needs no authorization while replying does.
const COMMUNITY_BASE_URL: &str = "https://mr.lbkrs.com/api/forward";
const TOPIC_PAGE_BASE_URL: &str = "https://longbridge.com/topics";
/// The page size longbridge.com uses for a profile's activity feed.
const FEED_PAGE_SIZE: usize = 25;
/// How many of the newest posts the feed keeps, so activities that are not posts, such as
/// follows, never push a post out.
const POST_COUNT: usize = 20;
/// Bounds the pages read for a member whose recent activity is mostly not posts.
const MAX_FEED_PAGES: usize = 4;
/// The tail mark of the feed's first page, and the one that marks its end.
const FIRST_PAGE_TAIL_MARK: &str = "0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const BLOCK_ELEMENTS: &[&str] = &[
    "p",
    "div",
    "li",
    "blockquote",
    "pre",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
];

pub struct SdkCommunityApi {
    content: ContentContext,
    public_http: reqwest::Client,
    limiter: Arc<RateLimiter>,
}

impl SdkCommunityApi {
    pub fn new(config: Arc<Config>, limiter: Arc<RateLimiter>) -> Result<Self> {
        let public_http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent(concat!("tickloop-ticks/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| Error::network("cannot create community HTTP client"))?;
        Ok(Self {
            content: ContentContext::new(config),
            public_http,
            limiter,
        })
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let response = self
            .public_http
            .get(format!("{COMMUNITY_BASE_URL}{path}"))
            .send()
            .await
            .map_err(|_| Error::network("community request failed"))?
            .error_for_status()
            .map_err(|error| {
                Error::network(format!(
                    "community request failed: {}",
                    error
                        .status()
                        .map_or_else(|| "unknown status".into(), |status| status.to_string())
                ))
            })?;
        let envelope: Envelope<T> = response
            .json()
            .await
            .map_err(|_| Error::network("invalid community response"))?;
        if envelope.code != 0 {
            return Err(Error::network(format!(
                "community request failed: {}",
                envelope.message
            )));
        }
        envelope
            .data
            .ok_or_else(|| Error::network("community response has no data"))
    }
}

#[async_trait]
impl CommunityApi for SdkCommunityApi {
    async fn profile(&self, member_id: i64) -> Result<MemberProfile> {
        let data: ProfileData = self
            .get(&format!("/social/profile?member_id={member_id}"))
            .await?;
        let profile = data
            .profile
            .ok_or_else(|| Error::usage(format!("community member {member_id} not found")))?;
        Ok(MemberProfile {
            member_id: parse_id(&profile.member_id, "member")?,
            nickname: profile.nickname,
            description: profile.description,
        })
    }

    async fn posts(&self, member_id: i64) -> Result<Vec<MemberPost>> {
        let mut posts = Vec::new();
        let mut tail_mark = FIRST_PAGE_TAIL_MARK.to_owned();
        for _ in 0..MAX_FEED_PAGES {
            let page: FeedData = self
                .get(&format!(
                    "/v2/social/profiles/{member_id}/activities?limit={FEED_PAGE_SIZE}&tail_mark={tail_mark}"
                ))
                .await?;
            let is_last_page = page.activities.is_empty();
            posts.extend(member_posts(page.activities)?);
            if posts.len() >= POST_COUNT {
                break;
            }
            match page.next_params.and_then(|next| next.tail_mark) {
                Some(next) if !is_last_page && next != FIRST_PAGE_TAIL_MARK => tail_mark = next,
                _ => break,
            }
        }
        posts.truncate(POST_COUNT);
        Ok(posts)
    }

    async fn reply(&self, topic_id: i64, body: &str) -> Result<PostedReply> {
        // A reply is not idempotent, so it is sent once and never retried.
        self.limiter.acquire().await;
        let reply = self
            .content
            .create_topic_reply(
                topic_id.to_string(),
                CreateReplyOptions {
                    body: body.to_owned(),
                    reply_to_id: None,
                },
            )
            .await
            .map_err(reply_error)?;
        posted_reply(reply)
    }
}

/// A reply whose request may have reached the server without a definite answer has an unknown
/// outcome; a refusal the server stated, or a request that never left, is a known failure.
fn reply_error(error: longbridge::Error) -> Error {
    let is_outcome_unknown = match &error {
        longbridge::Error::HttpClient(
            HttpClientError::RequestTimeout | HttpClientError::DeserializeResponseBody(_),
        ) => true,
        longbridge::Error::HttpClient(HttpClientError::Http(http)) => !http.0.is_connect(),
        longbridge::Error::HttpClient(
            HttpClientError::BadStatus(status)
            | HttpClientError::UnexpectedHttpResponse { status, .. },
        ) => status.is_server_error(),
        _ => false,
    };
    if is_outcome_unknown {
        reply_outcome_unknown(error)
    } else {
        crate::sdk::sdk_error(error)
    }
}

fn posted_reply(reply: TopicReply) -> Result<PostedReply> {
    Ok(PostedReply {
        reply_id: parse_id(&reply.id, "reply")?,
        topic_id: parse_id(&reply.topic_id, "topic")?,
        body: reply.body,
        created_at: reply
            .created_at
            .format(&Rfc3339)
            .map_err(|_| Error::network("invalid reply time"))?,
    })
}

/// The member's own posts among the activities, newest first. Activities that carry no post,
/// such as follows, are not part of it, and neither are their targets decoded.
fn member_posts(activities: Vec<ActivityPayload>) -> Result<Vec<MemberPost>> {
    activities
        .into_iter()
        .filter_map(|activity| {
            let kind = match activity.action.as_str() {
                "CreateTweet" => PostKind::Post,
                "CreatePost" => PostKind::Article,
                "Quote" | "QuoteNews" => PostKind::Quote,
                _ => return None,
            };
            let target = activity.targets.into_iter().find(|target| {
                target["_type"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with(".Article"))
            })?;
            Some(member_post(kind, target))
        })
        .collect()
}

fn member_post(kind: PostKind, target: Value) -> Result<MemberPost> {
    let article: ArticlePayload = serde_json::from_value(target)
        .map_err(|error| Error::network(format!("invalid community post: {error}")))?;
    let topic_id = parse_id(&article.id, "topic")?;
    let published_at = article
        .published_at
        .parse()
        .ok()
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|time| time.format(&Rfc3339).ok())
        .ok_or_else(|| Error::network("invalid post publication time"))?;
    Ok(MemberPost {
        topic_id,
        kind,
        title: article.title.unwrap_or_default().trim().to_owned(),
        text: plain_text(&article.description_html.unwrap_or_default()),
        published_at,
        url: format!("{TOPIC_PAGE_BASE_URL}/{topic_id}"),
    })
}

fn parse_id(id: &str, what: &str) -> Result<i64> {
    id.parse()
        .map_err(|_| Error::network(format!("invalid community {what} ID {id:?}")))
}

/// The post as plain text: each block element and line break starts a new line, and a
/// security tag keeps its visible text such as `$Tesla(TSLA.US)`.
fn plain_text(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    let mut lines = vec![String::new()];
    for node in fragment.root_element().descendants() {
        match node.value() {
            scraper::Node::Text(text) => lines
                .last_mut()
                .expect("lines starts non-empty")
                .push_str(text),
            scraper::Node::Element(element)
                if element.name() == "br" || BLOCK_ELEMENTS.contains(&element.name()) =>
            {
                lines.push(String::new());
            }
            _ => (),
        }
    }
    lines
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct Envelope<T> {
    code: i64,
    #[serde(default)]
    message: String,
    data: Option<T>,
}

#[derive(Deserialize)]
struct ProfileData {
    profile: Option<ProfilePayload>,
}

#[derive(Deserialize)]
struct ProfilePayload {
    member_id: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    description: String,
}

#[derive(Deserialize)]
struct FeedData {
    #[serde(default)]
    activities: Vec<ActivityPayload>,
    next_params: Option<NextPage>,
}

#[derive(Deserialize)]
struct NextPage {
    tail_mark: Option<String>,
}

#[derive(Deserialize)]
struct ActivityPayload {
    action: String,
    #[serde(default)]
    targets: Vec<Value>,
}

#[derive(Deserialize)]
struct ArticlePayload {
    id: String,
    title: Option<String>,
    description_html: Option<String>,
    published_at: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"[
        {"action": "CreateTweet", "targets": [{"_type": "type.googleapis.com/lb.social.service.Article",
            "id": "44219307", "title": "", "published_at": "1790031917",
            "description_html": "<p><span type=\"security-tag\" counter_id=\"ST/US/WDC\">$西部数据(WDC.US)</span>发生了什么</p><p></p><p>第二段<br>换行</p>"}]},
        {"action": "Follow", "targets": [{"_type": "type.googleapis.com/lb.social.service.CommonProfile",
            "id": 1, "title": null}]},
        {"action": "CreatePost", "targets": [{"_type": "type.googleapis.com/lb.social.service.Article",
            "id": "26177805", "title": " 重磅官宣 ", "published_at": "1734588576",
            "description_html": "<h2>小标题</h2><p>敬亲爱的桥友们...</p>"}]},
        {"action": "QuoteNews", "targets": [{"_type": "type.googleapis.com/lb.social.service.Article",
            "id": "6993508780031016960", "title": null, "published_at": "1717654718",
            "description_html": "plain"}]}
    ]"#;

    fn activities(feed: &str) -> Vec<ActivityPayload> {
        serde_json::from_str(feed).unwrap()
    }

    #[test]
    fn the_feed_keeps_the_members_posts_in_order_as_plain_text() {
        let posts = member_posts(activities(FEED)).unwrap();
        assert_eq!(
            posts,
            [
                MemberPost {
                    topic_id: 44219307,
                    kind: PostKind::Post,
                    title: String::new(),
                    text: "$西部数据(WDC.US)发生了什么\n第二段\n换行".into(),
                    published_at: "2026-09-21T23:05:17Z".into(),
                    url: "https://longbridge.com/topics/44219307".into(),
                },
                MemberPost {
                    topic_id: 26177805,
                    kind: PostKind::Article,
                    title: "重磅官宣".into(),
                    text: "小标题\n敬亲爱的桥友们...".into(),
                    published_at: "2024-12-19T06:09:36Z".into(),
                    url: "https://longbridge.com/topics/26177805".into(),
                },
                MemberPost {
                    topic_id: 6993508780031016960,
                    kind: PostKind::Quote,
                    title: String::new(),
                    text: "plain".into(),
                    published_at: "2024-06-06T06:18:38Z".into(),
                    url: "https://longbridge.com/topics/6993508780031016960".into(),
                },
            ]
        );
    }

    #[test]
    fn a_post_with_a_malformed_id_fails_the_feed() {
        let feed = FEED.replace("44219307", "not-a-number");
        let error = member_posts(activities(&feed)).unwrap_err();
        assert!(error.message.contains("not-a-number"), "{error:?}");
    }

    #[test]
    fn a_posted_reply_keeps_its_ids_and_time() {
        let reply: TopicReply = serde_json::from_value(serde_json::json!({
            "id": "48453318", "topic_id": "44033549", "body": "Nice", "reply_to_id": "0",
            "author": {"member_id": "91", "name": "资深韭菜", "avatar": ""},
            "created_at": "1791974002",
        }))
        .unwrap();
        assert_eq!(
            posted_reply(reply).unwrap(),
            PostedReply {
                reply_id: 48453318,
                topic_id: 44033549,
                body: "Nice".into(),
                created_at: "2026-10-14T10:33:22Z".into(),
            }
        );
    }

    #[test]
    fn only_a_reply_without_a_definite_answer_has_an_unknown_outcome() {
        let unknown = reply_error(longbridge::Error::HttpClient(
            HttpClientError::RequestTimeout,
        ));
        assert!(unknown.message.contains("outcome unknown"), "{unknown:?}");
        let refused = reply_error(longbridge::Error::HttpClient(HttpClientError::OpenApi {
            code: 403,
            message: "no assets".into(),
            trace_id: String::new(),
        }));
        assert!(!refused.message.contains("outcome unknown"), "{refused:?}");
        assert!(refused.message.contains("no assets"), "{refused:?}");
    }
}
