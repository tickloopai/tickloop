use std::collections::VecDeque;

pub(super) struct ReplyLines {
    pub(super) chat_id: uuid::Uuid,
    pub(super) message_id: Option<String>,
    incomplete: String,
    complete: VecDeque<String>,
    started: bool,
}

impl ReplyLines {
    pub(super) fn new(chat_id: uuid::Uuid, message_id: Option<String>) -> Self {
        Self {
            chat_id,
            message_id,
            incomplete: String::new(),
            complete: VecDeque::new(),
            started: false,
        }
    }

    pub(super) fn push(&mut self, delta: &str) {
        for segment in delta.split_inclusive('\n') {
            self.incomplete.push_str(segment);
            if segment.ends_with('\n') {
                self.complete
                    .push_back(std::mem::take(&mut self.incomplete));
            }
        }
    }

    pub(super) fn finish(&mut self) {
        if !self.incomplete.is_empty() {
            self.complete
                .push_back(std::mem::take(&mut self.incomplete));
        }
    }

    pub(super) fn has_complete_line(&self) -> bool {
        !self.complete.is_empty()
    }

    pub(super) fn next_line(&mut self) -> Option<(String, bool)> {
        self.complete.pop_front().map(|line| {
            let starts_message = !self.started;
            self.started = true;
            (line, starts_message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_complete_lines_and_releases_one_at_a_time() {
        let mut reply = ReplyLines::new(uuid::Uuid::nil(), Some("reply".into()));
        reply.push("first li");
        assert!(!reply.has_complete_line());
        reply.push("ne\nsecond\nthird");
        assert_eq!(reply.next_line(), Some(("first line\n".into(), true)));
        assert_eq!(reply.next_line(), Some(("second\n".into(), false)));
        assert!(!reply.has_complete_line());
        reply.finish();
        assert_eq!(reply.next_line(), Some(("third".into(), false)));
    }

    #[test]
    fn completion_releases_a_reply_without_a_newline() {
        let mut reply = ReplyLines::new(uuid::Uuid::nil(), None);
        reply.push("one ");
        reply.push("line");
        reply.finish();
        assert_eq!(reply.next_line(), Some(("one line".into(), true)));
    }
}
