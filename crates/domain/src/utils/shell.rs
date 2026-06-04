//! Shell helpers for provider commands executed inside compute instances.

use crate::ports::database_provider::ProviderError;

/// Wrap `sql` in a POSIX heredoc body suitable for `$(cat <<'DELIM' … DELIM)`.
pub fn sql_heredoc_body(delimiter: &str, sql: &str) -> Result<String, ProviderError> {
    if sql.contains(delimiter) {
        return Err(ProviderError::InvalidParams(format!(
            "SQL must not contain delimiter '{delimiter}'"
        )));
    }
    Ok(format!(
        "$(cat <<'{delimiter}'\n{sql}\n{delimiter}\n)"
    ))
}
