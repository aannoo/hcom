use std::collections::HashSet;

pub(super) fn parse_outbound_message(input: &str) -> (Vec<String>, String) {
    let mut recipients: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut body_tokens: Vec<String> = Vec::new();

    for token in input.split_whitespace() {
        if let Some((recipient, suffix)) = parse_recipient_token(token) {
            let recipient_name = recipient.to_string();
            if seen.insert(recipient_name.clone()) {
                recipients.push(recipient_name);
            }

            if !suffix.is_empty() {
                if let Some(last) = body_tokens.last_mut() {
                    last.push_str(suffix);
                } else {
                    body_tokens.push(suffix.to_string());
                }
            }
            continue;
        }

        body_tokens.push(token.to_string());
    }

    (recipients, body_tokens.join(" "))
}

fn parse_recipient_token(token: &str) -> Option<(&str, &str)> {
    if !token.starts_with('@') {
        return None;
    }

    let rest = &token[1..];
    if rest.is_empty() {
        return None;
    }

    let mut end = 0usize;
    for (idx, ch) in rest.char_indices() {
        if !is_recipient_char(ch) {
            break;
        }
        end = idx + ch.len_utf8();
    }

    if end == 0 {
        return None;
    }

    let recipient = &rest[..end];
    let suffix = &rest[end..];
    Some((recipient, suffix))
}

pub(crate) fn is_recipient_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-' | ':')
}

#[cfg(test)]
mod tests {
    use super::parse_outbound_message;

    #[test]
    fn parses_mentions_and_body() {
        let (recipients, body) = parse_outbound_message("@luna hello team");
        assert_eq!(recipients, vec!["luna"]);
        assert_eq!(body, "hello team");
    }

    #[test]
    fn preserves_punctuation_in_body() {
        let (recipients, body) = parse_outbound_message("hey @luna, ping @rova:BOXE.");
        assert_eq!(recipients, vec!["luna", "rova:BOXE"]);
        assert_eq!(body, "hey, ping.");
    }

    #[test]
    fn treats_at_symbol_without_name_as_body_text() {
        let (recipients, body) = parse_outbound_message("@ hello");
        assert!(recipients.is_empty());
        assert_eq!(body, "@ hello");
    }

    #[test]
    fn deduplicates_recipients() {
        let (recipients, body) = parse_outbound_message("@luna @luna hi");
        assert_eq!(recipients, vec!["luna"]);
        assert_eq!(body, "hi");
    }
}
