use tickloop_harness::model::ModelRequest;

/// Asserts that `later` resends the conversation of `earlier` unchanged, so a provider's prompt
/// cache still holds it.
pub fn assert_extends(earlier: &ModelRequest, later: &ModelRequest) {
    let conversation = |request: &ModelRequest| {
        request
            .messages
            .iter()
            .filter(|message| message.role != tickloop_harness::protocol::Role::System)
            .map(|message| serde_json::to_value(message).unwrap())
            .collect::<Vec<_>>()
    };
    let (earlier, later) = (conversation(earlier), conversation(later));
    assert!(later.len() > earlier.len());
    assert_eq!(&later[..earlier.len()], earlier.as_slice());
}

pub fn mentions(request: &ModelRequest, text: &str) -> usize {
    serde_json::to_string(&request.messages)
        .unwrap()
        .matches(text)
        .count()
}

/// Whether the request's history leaves the guidance of the Skill `name` in effect: a guidance
/// note for it that no later host note withdraws.
pub fn guidance_in_effect(request: &ModelRequest, name: &str) -> bool {
    const WITHDRAWN: &str = "Host notice: the guidance from these Skills above no longer applies; do not follow it, and load a Skill again if the task still needs it: ";
    let announced = format!("Reference guidance from {name} active in this Chat");
    let mut in_effect = false;
    for text in request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            tickloop_harness::protocol::Content::Text { text } => Some(text),
            _ => None,
        })
    {
        if text.starts_with(&announced) {
            in_effect = true;
        } else if let Some((withdrawn, _)) = text
            .strip_prefix(WITHDRAWN)
            .and_then(|rest| rest.split_once('.'))
            && withdrawn.split(", ").any(|withdrawn| withdrawn == name)
        {
            in_effect = false;
        }
    }
    in_effect
}
