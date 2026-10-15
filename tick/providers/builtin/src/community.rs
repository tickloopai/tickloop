use async_trait::async_trait;
use tick_provider::{
    community::{CommunityApi, MemberPost, MemberProfile, PostKind, PostedReply},
    error::{Error, Result},
};

const DEMO_REPLY_ID: i64 = 900_001;
const DEMO_REPLY_TIME: &str = "2026-09-10T10:00:00Z";

/// A simulated community member whose posts never change; a reply is accepted but goes nowhere.
pub struct DemoCommunity;

#[async_trait]
impl CommunityApi for DemoCommunity {
    async fn profile(&self, member_id: i64) -> Result<MemberProfile> {
        Ok(MemberProfile {
            member_id,
            nickname: "Demo · 示例用户".into(),
            description: "模拟的社区用户，不代表真实观点。".into(),
        })
    }

    async fn posts(&self, _: i64) -> Result<Vec<MemberPost>> {
        Ok(vec![
            MemberPost {
                topic_id: 1003,
                kind: PostKind::Post,
                title: String::new(),
                text: "$Demo · Tesla(TSLA.US) 今天放量突破，先看能否站稳前高。\n仅为示例内容。"
                    .into(),
                published_at: "2026-09-10T09:30:00Z".into(),
                url: "https://example.com/topics/1003".into(),
            },
            MemberPost {
                topic_id: 1002,
                kind: PostKind::Article,
                title: "Demo · 季度复盘：现金流比利润更值得看".into(),
                text: "这是一篇模拟长文的摘要，用于检查有限 Panel 中的换行与滚动。".into(),
                published_at: "2026-09-09T12:00:00Z".into(),
                url: "https://example.com/topics/1002".into(),
            },
            MemberPost {
                topic_id: 1001,
                kind: PostKind::Quote,
                title: String::new(),
                text: "Demo · 引用一条资讯：先等财报再说。".into(),
                published_at: "2026-09-08T08:00:00Z".into(),
                url: "https://example.com/topics/1001".into(),
            },
        ])
    }

    async fn reply(&self, topic_id: i64, body: &str) -> Result<PostedReply> {
        if body.trim().is_empty() {
            return Err(Error::usage("a reply needs text"));
        }
        Ok(PostedReply {
            reply_id: DEMO_REPLY_ID,
            topic_id,
            body: body.to_owned(),
            created_at: DEMO_REPLY_TIME.into(),
        })
    }
}
