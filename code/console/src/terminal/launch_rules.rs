use tickloop_harness::protocol::{Decision, PermissionRule};
use tickloop_harness::tools::permissions::validate_rule;

pub(super) fn parse(value: &str, decision: Decision) -> Result<Vec<PermissionRule>, String> {
    let invalid = || {
        "invalid permission rule: use Tool or Tool(content), separated by spaces or commas"
            .to_owned()
    };
    let mut parentheses = Vec::new();
    let mut backticks = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut tokens = Vec::new();
    let mut characters = value.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if !parentheses.is_empty() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' && quote != Some('\'') {
                escaped = true;
                continue;
            }
            if quote != Some('\'') {
                if character == '$' && characters.peek().is_some_and(|(_, next)| *next == '(') {
                    characters.next();
                    parentheses.push(quote.take());
                    continue;
                }
                if character == '`' {
                    if backticks
                        .last()
                        .is_some_and(|(depth, _)| *depth == parentheses.len())
                    {
                        quote = backticks.pop().expect("matched backtick").1;
                    } else {
                        backticks.push((parentheses.len(), quote.take()));
                    }
                    continue;
                }
            }
            if let Some(active_quote) = quote {
                if character == active_quote {
                    quote = None;
                }
                continue;
            }
            if matches!(character, '\'' | '"') {
                quote = Some(character);
                continue;
            }
        }
        match character {
            '(' => {
                if parentheses.is_empty() && value[start..index].contains(')') {
                    return Err(invalid());
                }
                parentheses.push(None);
            }
            ')' => quote = parentheses.pop().ok_or_else(invalid)?,
            ',' | ' ' | '\t' | '\n' | '\r' if parentheses.is_empty() => {
                if start < index {
                    tokens.push(&value[start..index]);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if !parentheses.is_empty() || !backticks.is_empty() {
        return Err(invalid());
    }
    if start < value.len() {
        tokens.push(&value[start..]);
    }
    if tokens.is_empty() {
        return Err(invalid());
    }
    tokens
        .into_iter()
        .map(|token| {
            let (tool, specifier) = match token.split_once('(') {
                Some((tool, content)) => {
                    let content = content.strip_suffix(')').ok_or_else(invalid)?;
                    if content.trim().is_empty() {
                        return Err(invalid());
                    }
                    (tool, Some(content.to_owned()))
                }
                None => (token, None),
            };
            if tool.is_empty()
                || !tool
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'*'))
            {
                return Err(invalid());
            }
            let rule = PermissionRule {
                id: tickloop_harness::ids::generate(),
                decision,
                tool: tool.into(),
                specifier,
                input: None,
                loop_id: None,
                persistent: false,
            };
            validate_rule(&rule).map_err(|reason| format!("invalid permission rule: {reason}"))?;
            Ok(rule)
        })
        .collect()
}
