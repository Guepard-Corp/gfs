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
        .ok_or_else(|| ParseRemoteSourceError::Invalid(
            "remote URL must start with postgres:// or postgresql://".into(),
        ))?;

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
        ParseRemoteSourceError::Invalid("remote URL must include a database name (.../dbname)".into())
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

    Ok(RemoteSource {
        host,
        port,
        dbname: dbname.to_string(),
        user,
        password,
        schemas,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let r =
            parse_postgres_url("postgresql://bob@localhost/analytics?schema=reporting,staging")
                .unwrap();
        assert_eq!(r.port, 5432);
        assert_eq!(r.password, "");
        assert_eq!(
            r.schemas,
            vec!["reporting".to_string(), "staging".to_string()]
        );
    }
}
