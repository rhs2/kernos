//! Path globbing for `action.touches_path(glob)`.

/// Matches a slash-separated path against a glob where `*` matches any run of
/// characters inside one segment and `**` matches zero or more whole segments.
/// Exists because policies gate on file paths (`infra/**`) and the semantics must
/// be identical in every kernel build.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match_segments(&pattern_segments, &path_segments)
}

fn match_segments(pattern: &[&str], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            // `**` may swallow zero or more segments.
            (0..=path.len()).any(|skip| match_segments(rest, &path[skip..]))
        }
        Some((first, rest)) => match path.split_first() {
            None => false,
            Some((segment, path_rest)) => {
                segment_match(first.as_bytes(), segment.as_bytes())
                    && match_segments(rest, path_rest)
            }
        },
    }
}

/// Matches one segment against a segment pattern where `*` matches any run of
/// bytes (including none).
fn segment_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|skip| segment_match(rest, &text[skip..])),
        Some((byte, rest)) => match text.split_first() {
            Some((t, text_rest)) if t == byte => segment_match(rest, text_rest),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn double_star_crosses_segments() {
        assert!(glob_match("infra/**", "infra/main.tf"));
        assert!(glob_match("infra/**", "infra/modules/net/main.tf"));
        assert!(glob_match("infra/**", "infra"));
        assert!(!glob_match("infra/**", "app/infra/main.tf"));
        assert!(glob_match("**/infra/**", "app/infra/main.tf"));
    }

    #[test]
    fn single_star_stays_inside_a_segment() {
        assert!(glob_match("infra/*.tf", "infra/main.tf"));
        assert!(!glob_match("infra/*.tf", "infra/modules/main.tf"));
        assert!(glob_match("*", "readme"));
        assert!(!glob_match("*", "a/b"));
        assert!(glob_match("src/*/mod.rs", "src/net/mod.rs"));
    }

    #[test]
    fn literal_paths() {
        assert!(glob_match("a/b/c", "a/b/c"));
        assert!(!glob_match("a/b/c", "a/b"));
        assert!(!glob_match("a/b", "a/b/c"));
        assert!(glob_match("", ""));
    }
}
