//! Parse `postgres://` URLs into [`RemoteSource`] for lazy clone.

use crate::ports::database_provider::RemoteSource;

#[derive(Debug, thiserror::Error)]
pub enum ParseRemoteSourceError {
    #[error("{0}")]
    Invalid(String),
}

/// Parse `postgres://user:password@host:port/dbname[?schema=...]` into a
/// [`RemoteSource`]. Keeps parsing intentionally simple (no percent-decoding).
pub fn parse_postgres_url(url: &str) -> Result<RemoteSource, ParseRemoteSourceError> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            ParseRemoteSourceError::Invalid(
                "remote URL must start with postgres:// or postgresql://".into(),
            )
        })?;

    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };

    let (userinfo, hostpart) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };

    let (user, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (ui.to_string(), String::new()),
        },
        None => {
            return Err(ParseRemoteSourceError::Invalid(
                "remote URL must include credentials (user[:password]@)".into(),
            ));
        }
    };

    let (hostport, dbname) = hostpart.split_once('/').ok_or_else(|| {
        ParseRemoteSourceError::Invalid(
            "remote URL must include a database name (.../dbname)".into(),
        )
    })?;
    if dbname.is_empty() {
        return Err(ParseRemoteSourceError::Invalid(
            "remote URL must include a database name (.../dbname)".into(),
        ));
    }

    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| ParseRemoteSourceError::Invalid(format!("invalid port: '{p}'")))?,
        ),
        None => (hostport.to_string(), 5432),
    };
    if host.is_empty() {
        return Err(ParseRemoteSourceError::Invalid(
            "remote URL must include a host".into(),
        ));
    }

    let schemas = query
        .and_then(|q| {
            q.split('&').find_map(|kv| {
                kv.strip_prefix("schema=")
                    .or_else(|| kv.strip_prefix("schemas="))
            })
        })
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let sslmode = query.and_then(|q| {
        q.split('&').find_map(|kv| {
            kv.strip_prefix("sslmode=")
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        })
    });

    // Reject shell metacharacters at the boundary. Callers quote these before
    // building commands, but the fields are also carried into a task pod's `sh -c`
    // and into SQL literals by more than one caller, so refusing them here means a
    // hostile URL fails loudly at parse time instead of relying on every downstream
    // site remembering to quote. The password is deliberately exempt: it is always
    // passed via PGPASSWORD or a quoted literal, and restricting it would reject
    // legitimate generated secrets.
    reject_unsafe_field("host", &host)?;
    reject_unsafe_field("user", &user)?;
    reject_unsafe_field("database name", dbname)?;
    for s in &schemas {
        reject_unsafe_field("schema", s)?;
    }
    if let Some(m) = &sslmode {
        reject_unsafe_field("sslmode", m)?;
    }

    Ok(RemoteSource {
        host,
        port,
        dbname: dbname.to_string(),
        user,
        password,
        schemas,
        sslmode,
    })
}

/// Refuse a remote-source field that could change the meaning of a shell command
/// or a SQL literal it is interpolated into.
///
/// Denylist rather than allowlist: database and role names legitimately carry a
/// wide range of characters, and an allowlist tight enough to be safe would reject
/// real ones. What is refused is the set that is never valid in a host, role or
/// database name AND is meaningful to `sh` or to SQL quoting.
fn reject_unsafe_field(field: &str, value: &str) -> Result<(), ParseRemoteSourceError> {
    const FORBIDDEN: &[char] = &[
        ';', '|', '&', '$', '`', '(', ')', '<', '>', '\\', '"', '\'', '*', '?', '!', '#', '\n',
        '\r', '\t',
    ];
    if let Some(bad) = value
        .chars()
        .find(|c| FORBIDDEN.contains(c) || c.is_control())
    {
        return Err(ParseRemoteSourceError::Invalid(format!(
            "remote {field} contains an unsupported character {bad:?}; \
             host, user, database and schema names may not carry shell or quoting metacharacters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_shell_metacharacters_in_remote_fields() {
        // The exact shape that executed a command in the bootstrap task pod: the
        // dbname carried `; <cmd>; #`, and the whole string was interpolated into
        // an `sh -c` command.
        let injected = "postgres://u:p@host:5432/db; echo PWNED; #";
        let err = parse_postgres_url(injected).expect_err("must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported character"),
            "error must name the cause: {msg}"
        );

        // Each field independently.
        for url in [
            "postgres://u:p@ho$t:5432/db",
            "postgres://u:p@host:5432/d`b`",
            "postgres://u|x:p@host:5432/db",
            "postgres://u:p@host:5432/db\nrm -rf /",
        ] {
            assert!(
                parse_postgres_url(url).is_err(),
                "must refuse metacharacters: {url}"
            );
        }
    }

    #[test]
    fn accepts_ordinary_remote_urls() {
        // The denylist must not reject real sources -- including the awkward
        // shape seen in testing: a role with an underscore, a hyphenated host,
        // and a query string.
        for url in [
            "postgres://user:pass@localhost:5432/mydb",
            "postgresql://app_user:s3cr3t@db.example-host.com:5432/appdb?sslmode=require",
            "postgres://u:p@192.0.2.10:5432/srcdb",
        ] {
            assert!(
                parse_postgres_url(url).is_ok(),
                "must accept an ordinary source: {url}"
            );
        }
    }

    #[test]
    fn a_password_may_contain_metacharacters() {
        // Generated secrets legitimately contain punctuation; the password is
        // never interpolated unquoted, so restricting it would reject real inputs.
        let r = parse_postgres_url("postgres://u:p$a`s|s@host:5432/db")
            .expect("password punctuation must be accepted");
        assert_eq!(r.password, "p$a`s|s");
    }

    #[test]
    fn parses_full_url() {
        let r = parse_postgres_url("postgres://alice:s3cret@db.example.com:6543/shop").unwrap();
        assert_eq!(r.user, "alice");
        assert_eq!(r.password, "s3cret");
        assert_eq!(r.host, "db.example.com");
        assert_eq!(r.port, 6543);
        assert_eq!(r.dbname, "shop");
        assert!(r.schemas.is_empty());
    }

    #[test]
    fn defaults_port_and_parses_schemas() {
        let r = parse_postgres_url("postgresql://bob@localhost/analytics?schema=reporting,staging")
            .unwrap();
        assert_eq!(r.port, 5432);
        assert_eq!(r.password, "");
        assert_eq!(
            r.schemas,
            vec!["reporting".to_string(), "staging".to_string()]
        );
        assert!(r.sslmode.is_none());
    }

    #[test]
    fn parses_sslmode_query_param() {
        let r =
            parse_postgres_url("postgres://alice:s3cret@db.example.com:6543/shop?sslmode=require")
                .unwrap();
        assert_eq!(r.sslmode.as_deref(), Some("require"));
    }
}
