/// Validates that a user-supplied SQL string is a single, read-only SELECT
/// statement, before it ever reaches Postgres.
///
/// This exists because a naive `sql.starts_with("select")` check only
/// inspects the first word — it does nothing to stop a second statement
/// appended after a semicolon (`SELECT 1; DROP TABLE processes;`), since
/// the check never looks at the rest of the string.
///
/// This validator is a real (if intentionally simple) SQL tokenizer: it
/// walks the string tracking whether it's inside a single-quoted string,
/// a double-quoted identifier, a line comment (`--`), or a block comment
/// (`/* */`), and only treats a `;` as a statement separator when it's
/// outside all of those. That's what makes it safe against a semicolon
/// hidden inside a string literal or a comment, which a plain
/// `.contains(';')` check would either falsely reject or fail to catch.
///
/// This is a defense-in-depth layer, not the primary security boundary.
/// The primary boundary is the database role this connection runs as
/// (see migrations/0002_readonly_role.sql) — a role with SELECT-only
/// grants makes a write attempt fail at the database regardless of what
/// gets past this check. This validator exists to reject obviously bad
/// input early, with a clear error, before spending a round trip on it.
pub fn validate_single_select(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim();

    if trimmed.is_empty() {
        return Err("query cannot be empty".to_string());
    }

    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    let mut statement_count = 0usize;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut saw_non_trailing_content_after_semicolon = false;
    let mut first_keyword: Option<String> = None;

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();

        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if c == '*' && next == Some('/') {
                in_block_comment = false;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single_quote {
            if c == '\'' && next == Some('\'') {
                i += 2; // escaped '' inside a string literal
                continue;
            }
            if c == '\'' {
                in_single_quote = false;
            }
            i += 1;
            continue;
        }

        if in_double_quote {
            if c == '"' && next == Some('"') {
                i += 2; // escaped "" inside a quoted identifier
                continue;
            }
            if c == '"' {
                in_double_quote = false;
            }
            i += 1;
            continue;
        }

        // Not currently inside a string, identifier, or comment.
        match c {
            '\'' => {
                in_single_quote = true;
                i += 1;
            }
            '"' => {
                in_double_quote = true;
                i += 1;
            }
            '-' if next == Some('-') => {
                in_line_comment = true;
                i += 2;
            }
            '/' if next == Some('*') => {
                in_block_comment = true;
                i += 2;
            }
            ';' => {
                statement_count += 1;
                // Anything after this semicolon that isn't just trailing
                // whitespace/comments means a second real statement.
                let rest: String = chars[i + 1..].iter().collect();
                if !rest.trim().is_empty() {
                    saw_non_trailing_content_after_semicolon = true;
                }
                i += 1;
            }
            _ => {
                if first_keyword.is_none() && c.is_alphabetic() {
                    let start = i;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    first_keyword = Some(chars[start..i].iter().collect::<String>().to_lowercase());
                    continue;
                }
                i += 1;
            }
        }
    }

    if in_single_quote || in_double_quote {
        return Err("unterminated string literal or quoted identifier".to_string());
    }
    if in_block_comment {
        return Err("unterminated block comment".to_string());
    }

    if statement_count > 1 || saw_non_trailing_content_after_semicolon {
        return Err("only a single statement is allowed".to_string());
    }

    match first_keyword.as_deref() {
        Some("select") | Some("with") => {}
        _ => return Err("only SELECT (or WITH ... SELECT) statements are allowed".to_string()),
    }

    // Defense-in-depth keyword blocklist. This is deliberately a backstop,
    // not the primary defense (a determined bypass of a keyword list is
    // always possible in general) — the DB role is what actually enforces
    // this. Still worth rejecting obvious cases early with a clear message.
    const BLOCKED_KEYWORDS: &[&str] = &[
        "insert", "update", "delete", "drop", "alter", "truncate", "grant", "revoke", "create",
        "copy", "call", "execute", "vacuum", "reindex", "merge", "lock", "do", "set",
    ];

    let lower = trimmed.to_lowercase();
    for kw in BLOCKED_KEYWORDS {
        // Word-boundary check so e.g. "created_at" doesn't false-positive
        // on "create".
        for (idx, _) in lower.match_indices(kw) {
            let before_ok = idx == 0
                || !lower.as_bytes()[idx - 1].is_ascii_alphanumeric()
                    && lower.as_bytes()[idx - 1] != b'_';
            let after_idx = idx + kw.len();
            let after_ok = after_idx >= lower.len()
                || (!lower.as_bytes()[after_idx].is_ascii_alphanumeric()
                    && lower.as_bytes()[after_idx] != b'_');
            if before_ok && after_ok {
                return Err(format!("statement contains disallowed keyword: {kw}"));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_select() {
        assert!(validate_single_select("SELECT * FROM processes").is_ok());
    }

    #[test]
    fn accepts_select_with_trailing_semicolon() {
        assert!(validate_single_select("SELECT * FROM processes;").is_ok());
    }

    #[test]
    fn accepts_with_cte() {
        assert!(
            validate_single_select("WITH top AS (SELECT * FROM processes) SELECT * FROM top")
                .is_ok()
        );
    }

    #[test]
    fn rejects_stacked_statement() {
        let err = validate_single_select("SELECT 1; DROP TABLE processes;");
        assert!(err.is_err());
    }

    #[test]
    fn rejects_stacked_statement_no_trailing_semicolon_on_first() {
        let err = validate_single_select("SELECT 1; UPDATE processes SET owner='root'");
        assert!(err.is_err());
    }

    #[test]
    fn allows_semicolon_inside_string_literal() {
        // A single statement whose WHERE clause happens to compare against
        // a string containing a semicolon must not be falsely rejected.
        assert!(validate_single_select("SELECT * FROM processes WHERE name = 'foo;bar'").is_ok());
    }

    #[test]
    fn rejects_non_select_start() {
        assert!(validate_single_select("DROP TABLE processes").is_err());
    }

    #[test]
    fn rejects_blocked_keyword_even_as_subclause() {
        assert!(validate_single_select("SELECT * FROM processes; SELECT pg_sleep(1)").is_err());
    }

    #[test]
    fn does_not_false_positive_on_created_at_column() {
        assert!(
            validate_single_select("SELECT created_at FROM processes ORDER BY created_at").is_ok()
        );
    }

    #[test]
    fn rejects_unterminated_string() {
        assert!(
            validate_single_select("SELECT * FROM processes WHERE name = 'unterminated").is_err()
        );
    }
}
