//! YAML frontmatter, as utils/frontmatter.ts parses it.

use yaml_rust2::{Yaml, YamlLoader};

/// Frontmatter and body of a markdown file. `frontmatter` is `Yaml::Null` when there is none.
pub struct Parsed {
    pub frontmatter: Yaml,
    pub body: String,
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Split and parse frontmatter. Errors carry the YAML parser message.
pub fn parse(content: &str) -> Result<Parsed, String> {
    let normalized = normalize_newlines(crate::paths::strip_bom(content));
    let none = |body: String| {
        Ok(Parsed {
            frontmatter: Yaml::Null,
            body,
        })
    };
    if !normalized.starts_with("---") {
        return none(normalized);
    }
    let Some(end) = normalized[3..].find("\n---").map(|i| i + 3) else {
        return none(normalized);
    };
    // TS: slice(4, end); an empty YAML string counts as no frontmatter.
    let yaml = normalized.get(4..end).unwrap_or("");
    let body = normalized[end + 4..].trim().to_string();
    if yaml.is_empty() {
        return none(body);
    }
    let docs = YamlLoader::load_from_str(yaml).map_err(|e| e.to_string())?;
    Ok(Parsed {
        frontmatter: docs.into_iter().next().unwrap_or(Yaml::Null),
        body,
    })
}

/// The body with frontmatter removed (`stripFrontmatter`).
pub fn strip(content: &str) -> Result<String, String> {
    parse(content).map(|parsed| parsed.body)
}

/// A string field of a frontmatter mapping.
pub fn string_field<'a>(frontmatter: &'a Yaml, key: &str) -> Option<&'a str> {
    frontmatter[key].as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_frontmatter_splits_yaml_and_body() {
        let parsed =
            parse("\u{feff}---\r\nname: x\r\ndescription: |\r\n  two\r\n  lines\r\n---\r\n\r\nBody\r\n").unwrap();
        assert_eq!(string_field(&parsed.frontmatter, "name"), Some("x"));
        assert_eq!(string_field(&parsed.frontmatter, "description"), Some("two\nlines\n"));
        assert_eq!(parsed.body, "Body");
        let plain = parse("no frontmatter\n").unwrap();
        assert_eq!(plain.body, "no frontmatter\n");
        assert!(plain.frontmatter.is_null());
        assert!(parse("---\nname: [unclosed\n---\n").is_err());
    }
}
