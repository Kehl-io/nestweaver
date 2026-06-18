pub fn split_identifier(fqn: &str) -> String {
    let mut words = Vec::new();
    for segment in fqn.split(|c: char| c == ':' || c == '.' || c == '/' || c == '\\') {
        if segment.is_empty() {
            continue;
        }
        split_camel_snake(segment, &mut words);
    }
    words.join(" ").to_lowercase()
}

fn split_camel_snake(s: &str, out: &mut Vec<String>) {
    for part in s.split('_') {
        if part.is_empty() {
            continue;
        }
        let mut current = String::new();
        let chars: Vec<char> = part.chars().collect();
        for i in 0..chars.len() {
            let c = chars[i];
            if c.is_uppercase() && !current.is_empty() {
                let next_is_lower = chars.get(i + 1).map_or(false, |n| n.is_lowercase());
                let prev_is_lower = i > 0 && chars[i - 1].is_lowercase();
                if prev_is_lower || next_is_lower {
                    out.push(current.clone());
                    current.clear();
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
}

pub fn symbol_embed_text(kind: &str, fqn: &str, docstring: Option<&str>) -> String {
    let split = split_identifier(fqn);
    let kind_lower = kind.to_lowercase();
    match docstring {
        Some(doc) => {
            let truncated = &doc[..doc.len().min(256)];
            format!("{kind_lower} {split} {truncated}")
        }
        None => format!("{kind_lower} {split}"),
    }
}

pub fn note_embed_text(title: &str, body: Option<&str>) -> String {
    match body {
        Some(b) => {
            let truncated = &b[..b.len().min(512)];
            format!("{title}\n{truncated}")
        }
        None => title.to_string(),
    }
}

pub fn heading_embed_text(note_title: &str, heading_text: &str) -> String {
    format!("{note_title} > {heading_text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_snake_case() {
        assert_eq!(split_identifier("validate_token"), "validate token");
    }

    #[test]
    fn test_split_camel_case() {
        assert_eq!(split_identifier("PaymentProcessor"), "payment processor");
    }

    #[test]
    fn test_split_namespace() {
        assert_eq!(
            split_identifier("auth::middleware::validate_token"),
            "auth middleware validate token"
        );
    }

    #[test]
    fn test_split_dotted() {
        assert_eq!(
            split_identifier("PaymentProcessor.processRefund"),
            "payment processor process refund"
        );
    }

    #[test]
    fn test_split_acronym() {
        assert_eq!(split_identifier("HTTPSConnection"), "https connection");
    }

    #[test]
    fn test_symbol_embed_text() {
        let text = symbol_embed_text("function", "auth::validate_token", Some("Validates JWT"));
        assert_eq!(text, "function auth validate token Validates JWT");
    }

    #[test]
    fn test_note_embed_text() {
        let text = note_embed_text("Architecture Overview", Some("This document describes..."));
        assert_eq!(text, "Architecture Overview\nThis document describes...");
    }

    #[test]
    fn test_heading_embed_text() {
        let text = heading_embed_text("Auth Guide", "Configuration");
        assert_eq!(text, "Auth Guide > Configuration");
    }
}
