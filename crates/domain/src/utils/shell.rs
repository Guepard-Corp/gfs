//! Shell helpers for provider commands executed inside compute instances.

use crate::ports::database_provider::ProviderError;

/// Wrap `sql` in a POSIX heredoc body suitable for `$(cat <<'DELIM' … DELIM)`.
pub fn sql_heredoc_body(delimiter: &str, sql: &str) -> Result<String, ProviderError> {
    if sql.contains(delimiter) {
        return Err(ProviderError::InvalidParams(format!(
            "SQL must not contain delimiter '{delimiter}'"
        )));
    }
    Ok(format!("$(cat <<'{delimiter}'\n{sql}\n{delimiter}\n)"))
}

/// Wrap `s` in single quotes for safe use in a `sh -c` command, escaping any
/// embedded single quote (`'` -> `'\''`).
pub fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
