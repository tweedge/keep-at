//! Keyword blocklist matched against title + description.

#[derive(Debug, Clone, Default)]
pub struct KeywordBlocklist {
    keywords: Vec<String>,
}

impl KeywordBlocklist {
    pub fn new(keywords: Vec<String>) -> KeywordBlocklist {
        KeywordBlocklist {
            keywords: keywords
                .into_iter()
                .map(|k| k.to_lowercase())
                .filter(|k| !k.is_empty())
                .collect(),
        }
    }

    /// Returns the blocking keyword if `title`/`description` matches.
    pub fn blocks(&self, title: &str, description: &str) -> Option<&str> {
        if self.keywords.is_empty() {
            return None;
        }
        let title = title.to_lowercase();
        let desc = description.to_lowercase();
        self.keywords
            .iter()
            .find(|k| title.contains(k.as_str()) || desc.contains(k.as_str()))
            .map(|k| k.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        let b = KeywordBlocklist::new(vec!["foo".to_string()]);
        assert_eq!(b.blocks("A Foo Tale", ""), Some("foo"));
        assert_eq!(b.blocks("ok", "has foo inside"), Some("foo"));
        assert_eq!(b.blocks("ok", "clean"), None);
        let empty = KeywordBlocklist::new(vec![]);
        assert_eq!(empty.blocks("foo", ""), None);
    }
}
