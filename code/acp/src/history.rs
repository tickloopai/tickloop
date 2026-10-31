//! Replay only public display nodes. Loading history never invokes a model or a tool.
use agent_client_protocol::{Client, ConnectionTo, Error};
use serde_json::{Value, json};
use tickloop_code_client::managed::ManagedSession;
use tickloop_harness::protocol::{ImageOrigin, ResourceContext};
use tickloop_harness::transcript::display::{DisplayBlock, DisplayKind, DisplayNode};
use tokio_util::sync::CancellationToken;

pub(super) async fn notify(
    connection: &ConnectionTo<Client>,
    session_id: &str,
    update: Value,
    budget: &super::transport::OutputBudget,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(super::invalid("session load cancelled")),
        result = budget.send_wait(connection, session_id, update) => result,
    }
}

pub(super) fn text_chunks(text: &str) -> impl Iterator<Item = &str> {
    struct Chunks<'a>(&'a str);
    impl<'a> Iterator for Chunks<'a> {
        type Item = &'a str;
        fn next(&mut self) -> Option<Self::Item> {
            if self.0.is_empty() {
                return None;
            }
            let mut end = self.0.len().min(16 * 1024);
            while !self.0.is_char_boundary(end) {
                end -= 1;
            }
            let (chunk, rest) = self.0.split_at(end);
            self.0 = rest;
            Some(chunk)
        }
    }
    Chunks(text)
}

async fn message(
    connection: &ConnectionTo<Client>,
    session_id: &str,
    kind: &str,
    content: Value,
    budget: &super::transport::OutputBudget,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    if content["type"] == "text" {
        for text in text_chunks(content["text"].as_str().unwrap_or_default()) {
            notify(
                connection,
                session_id,
                json!({"sessionUpdate":kind,"content":{"type":"text","text":text}}),
                budget,
                cancel,
            )
            .await?;
        }
        Ok(())
    } else {
        notify(
            connection,
            session_id,
            json!({"sessionUpdate":kind,"content":content}),
            budget,
            cancel,
        )
        .await
    }
}

fn resource_content(resource: ResourceContext) -> Value {
    match resource.content {
        Some(text) => {
            json!({"type":"resource","resource":{"uri":resource.uri,"mimeType":resource.mime_type,"text":text}})
        }
        None => {
            json!({"type":"resource_link","uri":resource.uri,"mimeType":resource.mime_type,"name":resource.name.unwrap_or_else(|| "Resource".into())})
        }
    }
}

pub(super) async fn replay(
    session: &ManagedSession,
    session_id: &str,
    cancel: &CancellationToken,
    connection: &ConnectionTo<Client>,
    budget: &super::transport::OutputBudget,
) -> Result<(), Error> {
    let mut cursor = None;
    let mut cursors = std::collections::HashSet::new();
    loop {
        let page = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(super::invalid("session load cancelled")),
            page = session.history(cursor) => page.map_err(super::access_error)?,
        };
        if page.session_id.to_string() != session_id {
            return Err(super::invalid("history session identity changed"));
        }
        for value in page.items {
            if cancel.is_cancelled() {
                return Err(super::invalid("session load cancelled"));
            }
            let node: DisplayNode =
                serde_json::from_value(value).map_err(Error::into_internal_error)?;
            let kind = match node.kind {
                DisplayKind::Prompt { .. } => "user_message_chunk",
                DisplayKind::Answer { .. } => "agent_message_chunk",
            };
            for block in node.blocks {
                match block {
                    DisplayBlock::Text { text } => {
                        message(
                            connection,
                            session_id,
                            kind,
                            json!({"type":"text","text":text}),
                            budget,
                            cancel,
                        )
                        .await?
                    }
                    DisplayBlock::Thinking { text } => {
                        message(
                            connection,
                            session_id,
                            "agent_thought_chunk",
                            json!({"type":"text","text":text}),
                            budget,
                            cancel,
                        )
                        .await?
                    }
                    DisplayBlock::Image(image) => {
                        let uri = match image.origin {
                            ImageOrigin::Attached { uri, .. } => uri,
                            ImageOrigin::Mentioned { .. }
                            | ImageOrigin::Pasted { .. }
                            | ImageOrigin::Later(_) => None,
                        };
                        message(connection, session_id, kind, json!({"type":"image","data":image.image.data,"mimeType":image.image.media_type,"uri":uri}), budget, cancel).await?;
                    }
                    DisplayBlock::Resource(resource) => {
                        message(
                            connection,
                            session_id,
                            kind,
                            resource_content(resource),
                            budget,
                            cancel,
                        )
                        .await?
                    }
                    DisplayBlock::ToolCall {
                        call_id,
                        name,
                        input,
                        result,
                        recorded_review,
                        ..
                    } => {
                        let locations = super::tool_locations(&name, &input);
                        notify(connection, session_id, json!({"sessionUpdate":"tool_call","toolCallId":call_id,"title":name,"kind":super::tool_kind(&name),"status":"pending","rawInput":super::bounded_value(input, super::MAX_TOOL_CONTENT),"locations":locations}), budget, cancel).await?;
                        if let Some(result) = result {
                            let text = if result.truncated {
                                format!("{}\n[Recorded display result is truncated]", result.text)
                            } else {
                                result.text
                            };
                            let mut content: Vec<_> = text_chunks(&text).map(|text| json!({"type":"content","content":{"type":"text","text":text}})).collect();
                            if let Some(review) = recorded_review {
                                content.push(json!({"type":"content","content":{"type":"text","text":"Recorded unified-diff hunks (not a complete before/after snapshot):"}}));
                                content.extend(text_chunks(&review.unified_diff).map(|text| json!({"type":"content","content":{"type":"text","text":text}})));
                            }
                            notify(connection, session_id, json!({"sessionUpdate":"tool_call_update","toolCallId":call_id,"status":if result.is_error {"failed"} else {"completed"},"content":content}), budget, cancel).await?;
                        } else {
                            notify(connection, session_id, json!({"sessionUpdate":"tool_call_update","toolCallId":call_id,"status":"failed","content":[{"type":"content","content":{"type":"text","text":"No terminal result was recorded; the operation was not replayed."}}]}), budget, cancel).await?;
                        }
                    }
                    // Managed ACP disallows these origins. Do not silently claim full replay.
                    DisplayBlock::Question { .. }
                    | DisplayBlock::Permission { .. }
                    | DisplayBlock::FollowUp { .. } => {
                        return Err(super::invalid("unsupported public history block"));
                    }
                }
                // Let the writer and cancel dispatcher drain; never enqueue a whole history at once.
                tokio::task::yield_now().await;
            }
        }
        match page.next_cursor {
            Some(next) if cursors.insert(next.clone()) => cursor = Some(next),
            Some(_) => return Err(super::invalid("history pagination cursor repeated")),
            None => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn utf8_chunks_are_lossless_and_bounded() {
        let text = "上下文".repeat(20_000);
        let chunks: Vec<_> = super::text_chunks(&text).collect();
        assert!(chunks.iter().all(|chunk| chunk.len() <= 16 * 1024));
        assert_eq!(chunks.concat(), text);
    }
}
