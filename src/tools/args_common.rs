//! Shell token helpers used by launch configuration and runner scripts.

/// Simple shell-safe quoting for runner script serialization.
pub fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/' || c == ':')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Split a configured argument string while preserving quoted values.
///
/// Outside quotes, `\` is a POSIX escape character (it consumes the next
/// character verbatim) on every platform except Windows, where it's treated
/// as a literal character instead — otherwise unquoted Windows paths like
/// `C:\Tools\term.exe` get mangled. This only affects the *unquoted* branch:
/// escapes inside quotes (`\"`, `\\`, `\$`, `` \` ``) are unchanged on every
/// platform, so quoted strings behave identically everywhere.
pub fn shell_split(s: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
        } else if in_double {
            if ch == '"' {
                in_double = false;
            } else if ch == '\\' {
                if let Some(&next) = chars.peek() {
                    if matches!(next, '"' | '\\' | '$' | '`') {
                        current.push(chars.next().unwrap());
                    } else {
                        current.push(ch);
                    }
                }
            } else {
                current.push(ch);
            }
        } else if ch == '\'' {
            in_single = true;
        } else if ch == '"' {
            in_double = true;
        } else if ch == '\\' {
            if cfg!(windows) {
                current.push(ch);
            } else if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }

    if in_single || in_double {
        return Err("unterminated quote".to_string());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_spaces_and_quotes() {
        assert_eq!(shell_quote("simple"), "simple");
        assert_eq!(shell_quote("has space"), "'has space'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_split_preserves_quoted_values() {
        assert_eq!(
            shell_split("--model opus --verbose").unwrap(),
            vec!["--model", "opus", "--verbose"]
        );
        assert_eq!(
            shell_split("'hello world' --flag").unwrap(),
            vec!["hello world", "--flag"]
        );
        assert!(shell_split("'unterminated").is_err());
    }

    #[test]
    fn shell_split_unquoted_backslash_is_platform_aware() {
        let result = shell_split(r"C:\Tools\term.exe --flag").unwrap();
        if cfg!(windows) {
            // Literal outside quotes on Windows, so real paths survive intact.
            assert_eq!(result, vec![r"C:\Tools\term.exe", "--flag"]);
        } else {
            // POSIX escape: each `\` consumes the following character.
            assert_eq!(result, vec!["C:Toolsterm.exe", "--flag"]);
        }
    }

    #[test]
    fn shell_split_quoted_escapes_are_platform_independent() {
        // Regression guard: a prior fix pre-doubled every backslash in the
        // whole input string before calling shell_split on Windows, which
        // corrupted quoted escapes like `\"` (they became `\\"`, closing the
        // quote early). The platform branch above only touches the *unquoted*
        // backslash case, so quoted escapes must behave identically everywhere.
        assert_eq!(
            shell_split(r#"myterm -e "say \"hi\"""#).unwrap(),
            vec!["myterm", "-e", "say \"hi\""]
        );
    }

    #[test]
    fn shell_split_quoted_windows_path_unaffected_by_platform() {
        // The quoted-escape branch is untouched by the Windows change, so a
        // quoted Windows path was never broken and stays that way.
        assert_eq!(
            shell_split(r#""C:\Tools\term.exe" --flag"#).unwrap(),
            vec![r"C:\Tools\term.exe", "--flag"]
        );
    }

    #[test]
    fn shell_split_double_backslash_no_longer_collapses_on_windows() {
        // A pre-existing (undocumented, code-comment-only) workaround relied
        // on shell_split's escape-collapse to turn a doubled backslash into a
        // single one on Windows. That collapse no longer happens outside
        // quotes on Windows (see above) — low-impact, since Windows path APIs
        // tolerate redundant separators, and the common single-backslash case
        // now works directly without any workaround.
        let result = shell_split(r"C:\\Tools\\term.exe").unwrap();
        if cfg!(windows) {
            assert_eq!(result, vec![r"C:\\Tools\\term.exe"]);
        } else {
            assert_eq!(result, vec![r"C:\Tools\term.exe"]);
        }
    }
}
