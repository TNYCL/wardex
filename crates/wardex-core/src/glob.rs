//! Shell-like name patterns for agent and task labels, shared by webhook
//! routing and session filters.

/// `*` matches any run of characters, `?` one character; the rest literally.
/// Case-sensitive.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// The same pattern for SQLite's `GLOB`, which also treats `[` as the start
/// of a character class.
pub(crate) fn to_sqlite(pattern: &str) -> String {
    pattern.replace('[', "[[]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("bot-a", "bot-a"));
        assert!(!glob("bot-a", "bot-a2"));
        assert!(glob("fix-*", "fix-login"));
        assert!(glob("fix-*", "fix-"));
        assert!(!glob("fix-*", "feat-login"));
        assert!(glob("*-login", "fix-login"));
        assert!(glob("b?t", "bot"));
        assert!(!glob("b?t", "boot"));
        assert!(glob("*", ""));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
        assert!(glob("[x]*", "[x]-1"));
        assert!(!glob("Bot", "bot"));
    }

    #[test]
    fn sqlite_patterns_keep_brackets_literal() {
        assert_eq!(to_sqlite("fix-*"), "fix-*");
        assert_eq!(to_sqlite("[x]?"), "[[]x]?");
    }
}
