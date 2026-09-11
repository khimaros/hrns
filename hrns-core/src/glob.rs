//! gitignore-style glob matching shared by the permission model and the
//! tool/hook/skill config gates. two modes: path mode (`*` stops at `/`, for
//! file paths) and command mode (`*` matches anything, for shell commands).

use std::collections::HashMap;

/// glob matching with optional path-aware semantics.
/// `**` always matches any sequence of characters.
/// in path mode: `*` matches non-`/` chars, `?` matches one non-`/` char.
/// in command mode: `*` and `?` match any character (including `/`).
/// returns a specificity score (higher = more literal, fewer wildcards) on a
/// match, so the most specific of several matching patterns can be chosen.
pub fn glob_matches(pattern: &str, input: &str, path_mode: bool) -> Option<usize> {
    if glob_matches_recursive(pattern.as_bytes(), input.as_bytes(), path_mode) {
        let specificity =
            pattern.len() - pattern.matches('*').count() - pattern.matches('?').count();
        Some(specificity)
    } else {
        None
    }
}

fn glob_matches_recursive(pattern: &[u8], input: &[u8], path_mode: bool) -> bool {
    match (pattern, input) {
        ([], []) => true,
        // `**` matches zero or more of anything
        ([b'*', b'*', rest @ ..], _) => {
            let rest = skip_stars(rest);
            for i in 0..=input.len() {
                if glob_matches_recursive(rest, &input[i..], path_mode) {
                    return true;
                }
            }
            false
        }
        // `*`: in path mode stops at `/`, otherwise matches anything
        ([b'*', rest @ ..], _) => {
            let rest = skip_stars(rest);
            for i in 0..=input.len() {
                if path_mode && i > 0 && input[i - 1] == b'/' {
                    break;
                }
                if glob_matches_recursive(rest, &input[i..], path_mode) {
                    return true;
                }
            }
            false
        }
        // `?`: in path mode skips non-`/`, otherwise any char
        ([b'?', rest @ ..], [c, input_rest @ ..]) if !path_mode || *c != b'/' => {
            glob_matches_recursive(rest, input_rest, path_mode)
        }
        ([p, rest @ ..], [c, input_rest @ ..]) if p == c => {
            glob_matches_recursive(rest, input_rest, path_mode)
        }
        _ => false,
    }
}

/// skips consecutive `*` characters in a pattern.
fn skip_stars(pattern: &[u8]) -> &[u8] {
    let mut p = pattern;
    while let [b'*', rest @ ..] = p {
        p = rest;
    }
    p
}

/// looks up `key` in a map keyed by gitignore-style globs. returns the
/// value of the most-specific matching pattern (longer literal prefix
/// wins; `"*"` has specificity 0 so it acts as a default). returns
/// `default` if nothing matches.
pub fn glob_lookup<T: Clone>(map: &HashMap<String, T>, key: &str, default: T) -> T {
    let mut best: Option<(usize, T)> = None;
    for (pattern, value) in map {
        if let Some(specificity) = glob_matches(pattern, key, false) {
            if best.as_ref().is_none_or(|(s, _)| specificity >= *s) {
                best = Some((specificity, value.clone()));
            }
        }
    }
    best.map(|(_, v)| v).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_exact_match() {
        assert!(glob_matches("apt update", "apt update", true).is_some());
        assert!(glob_matches("apt update", "apt upgrade", true).is_none());
    }

    #[test]
    fn test_glob_star_path_mode() {
        assert!(glob_matches("/etc/*", "/etc/passwd", true).is_some());
        // `*` should NOT cross `/` in path mode
        assert!(glob_matches("/etc/*", "/etc/ssh/config", true).is_none());
    }

    #[test]
    fn test_glob_star_command_mode() {
        // `*` crosses `/` in command mode
        assert!(glob_matches("ls *", "ls -la /home/user", false).is_some());
        assert!(glob_matches("*", "anything/with/slashes", false).is_some());
    }

    #[test]
    fn test_glob_doublestar_matches_across_slashes() {
        assert!(glob_matches("/home/**", "/home/user/photos/pic.jpg", true).is_some());
        assert!(glob_matches("/home/**/pic.jpg", "/home/user/photos/pic.jpg", true).is_some());
        assert!(glob_matches("**/*.jpg", "/home/user/pic.jpg", true).is_some());
    }

    #[test]
    fn test_glob_question_mark_path_mode() {
        assert!(glob_matches("file?.txt", "file1.txt", true).is_some());
        assert!(glob_matches("file?.txt", "file12.txt", true).is_none());
        // `?` should not match `/` in path mode
        assert!(glob_matches("a?b", "a/b", true).is_none());
    }

    #[test]
    fn test_glob_question_mark_command_mode() {
        // `?` matches `/` in command mode
        assert!(glob_matches("a?b", "a/b", false).is_some());
    }

    #[test]
    fn test_glob_bare_star_path_mode() {
        assert!(glob_matches("*", "anything", true).is_some());
        // bare `*` does not cross slashes in path mode
        assert!(glob_matches("*", "a/b", true).is_none());
        // but `**` does
        assert!(glob_matches("**", "a/b", true).is_some());
    }

    #[test]
    fn test_glob_specificity_ordering() {
        let exact = glob_matches("/etc/passwd", "/etc/passwd", true).unwrap();
        let glob = glob_matches("/etc/*", "/etc/passwd", true).unwrap();
        assert!(exact > glob);
    }
}
