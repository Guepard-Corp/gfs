//! Minimal PostgreSQL v3 wire-protocol helpers — just enough to negotiate the
//! startup phase and *observe* frontend queries (simple `Query` and extended
//! `Parse`). We never modify the byte stream; parsing is purely for telemetry
//! and warming. Inspired by guepard-proxy's `pq_proto`, trimmed to essentials.

/// Magic protocol codes carried in the startup packet's first int32.
pub const SSL_REQUEST_CODE: i32 = 80_877_103;
pub const GSSENC_REQUEST_CODE: i32 = 80_877_104;
pub const CANCEL_REQUEST_CODE: i32 = 80_877_102;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupKind {
    SslRequest,
    GssEncRequest,
    CancelRequest,
    /// A regular StartupMessage (carries the protocol version + parameters).
    Startup,
}

/// Try to read one startup-phase packet from `buf`.
///
/// Startup packets have **no type byte**: `int32 length` (incl. itself) then an
/// `int32` code/version. Returns `(kind, total_len)` once a full packet is
/// buffered, or `None` if more bytes are needed.
pub fn parse_startup(buf: &[u8]) -> Option<(StartupKind, usize)> {
    if buf.len() < 8 {
        return None;
    }
    let len = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len < 8 || buf.len() < len {
        return None;
    }
    let code = i32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let kind = match code {
        SSL_REQUEST_CODE => StartupKind::SslRequest,
        GSSENC_REQUEST_CODE => StartupKind::GssEncRequest,
        CANCEL_REQUEST_CODE => StartupKind::CancelRequest,
        _ => StartupKind::Startup, // protocol version, e.g. 196608 for 3.0
    };
    Some((kind, len))
}

/// A frontend (client→server) message we care to observe.
#[derive(Debug)]
pub enum Frontend<'a> {
    /// Simple query protocol: the raw SQL text.
    Query(&'a str),
    /// Extended protocol Parse: the prepared statement's name and SQL text
    /// (with `$n` placeholders).
    Parse { name: &'a str, query: &'a str },
    /// Extended protocol Bind: the statement name it binds, and the parameter
    /// values **iff** they are all text-format (`Some`); `None` if any parameter
    /// is binary-format (we can't cheaply decode those, so warming skips them).
    Bind { stmt: &'a str, params: Option<Vec<Option<String>>> },
    /// Any other typed message (Execute, Sync, Password, …).
    Other(u8),
}

/// Header of a typed frontend message: `(type, total_len_including_type)`.
/// Returns `None` if fewer than 5 bytes are buffered (header incomplete).
pub fn peek_header(buf: &[u8]) -> Option<(u8, usize)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize; // excludes type byte
    Some((buf[0], 1 + len))
}

/// Parse one complete typed frontend message at the front of `buf`.
/// Caller must ensure `buf` holds at least `peek_header().1` bytes.
pub fn parse_frontend(buf: &[u8]) -> Frontend<'_> {
    let typ = buf[0];
    let body = &buf[5..]; // payload after type(1) + len(4)
    match typ {
        b'Q' => Frontend::Query(cstr(body)),
        b'P' => Frontend::Parse {
            name: cstr(body),
            query: cstr(skip_cstr(body)),
        },
        b'B' => {
            let after_portal = skip_cstr(body); // skip portal name
            let stmt = cstr(after_portal);
            parse_bind_params(stmt, skip_cstr(after_portal))
        }
        other => Frontend::Other(other),
    }
}

/// Parse a Bind message's parameter section (after portal+statement names).
/// Returns text-format parameter values, or `None` params if any are binary.
fn parse_bind_params<'a>(stmt: &'a str, mut rest: &[u8]) -> Frontend<'a> {
    fn rd_i16(b: &mut &[u8]) -> Option<i16> {
        if b.len() < 2 { return None; }
        let v = i16::from_be_bytes([b[0], b[1]]);
        *b = &b[2..];
        Some(v)
    }
    fn rd_i32(b: &mut &[u8]) -> Option<i32> {
        if b.len() < 4 { return None; }
        let v = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        *b = &b[4..];
        Some(v)
    }
    let skip = || Frontend::Bind { stmt, params: None };

    let nfmt = match rd_i16(&mut rest) {
        Some(n) if n >= 0 => n as usize,
        _ => return skip(),
    };
    let mut fmts = Vec::with_capacity(nfmt);
    for _ in 0..nfmt {
        match rd_i16(&mut rest) {
            Some(c) => fmts.push(c),
            None => return skip(),
        }
    }
    let nparams = match rd_i16(&mut rest) {
        Some(n) if n >= 0 => n as usize,
        _ => return skip(),
    };
    let mut out = Vec::with_capacity(nparams);
    for i in 0..nparams {
        let code = match nfmt {
            0 => 0,                                   // all text
            1 => fmts[0],                             // one code for all
            _ => *fmts.get(i).unwrap_or(&0),          // per-param
        };
        let len = match rd_i32(&mut rest) {
            Some(l) => l,
            None => return skip(),
        };
        if len < 0 {
            out.push(None); // SQL NULL
            continue;
        }
        let len = len as usize;
        if rest.len() < len {
            return skip();
        }
        let val = &rest[..len];
        rest = &rest[len..];
        if code != 0 {
            return skip(); // binary parameter → can't substitute
        }
        out.push(Some(String::from_utf8_lossy(val).into_owned()));
    }
    Frontend::Bind { stmt, params: Some(out) }
}

/// Substitute `$1..$n` placeholders in `query` with quoted text literals (NULL
/// for missing values), producing a concrete SQL string to warm. Best-effort:
/// placeholders inside string/dollar-quoted literals are naively replaced too,
/// but the result is only ever EXPLAINed by the warmer (errors are harmless).
pub fn substitute_params(query: &str, params: &[Option<String>]) -> String {
    let mut out = String::with_capacity(query.len() + 16);
    let mut rest = query;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            out.push('$');
            rest = after;
            continue;
        }
        let n: usize = after[..digits].parse().unwrap_or(0);
        match params.get(n.wrapping_sub(1)) {
            Some(Some(v)) => {
                out.push('\'');
                out.push_str(&v.replace('\'', "''"));
                out.push('\'');
            }
            Some(None) => out.push_str("NULL"),
            None => {
                // Unknown placeholder: leave it (will just fail EXPLAIN harmlessly).
                out.push('$');
                out.push_str(&after[..digits]);
            }
        }
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// A backend `AuthenticationSASL` message (`'R'` + len + int32 `10` + mechanism
/// cstrings + empty terminator) advertising channel binding (`SCRAM-SHA-256-PLUS`)
/// breaks through a TLS-terminating proxy: the backend binds SCRAM to the
/// proxy↔backend TLS channel, but the client sees a different (or no) channel.
///
/// When the proxy↔backend link is TLS, rewrite the message to drop the `-PLUS`
/// mechanism so the client falls back to plain `SCRAM-SHA-256` (no binding),
/// which forwards transparently. Returns the rewritten full message, or `None`
/// if this isn't a SASL message or `-PLUS` isn't offered (forward verbatim).
pub fn strip_scram_plus(full_msg: &[u8]) -> Option<Vec<u8>> {
    if full_msg.first() != Some(&b'R') || full_msg.len() < 9 {
        return None;
    }
    let authtype = i32::from_be_bytes([full_msg[5], full_msg[6], full_msg[7], full_msg[8]]);
    if authtype != 10 {
        return None; // not AuthenticationSASL
    }
    let mut mechs: Vec<&[u8]> = Vec::new();
    let mut rest = &full_msg[9..];
    loop {
        let end = rest.iter().position(|&c| c == 0)?;
        if end == 0 {
            break; // empty string terminates the list
        }
        mechs.push(&rest[..end]);
        rest = &rest[end + 1..];
    }
    if !mechs.iter().any(|m| *m == b"SCRAM-SHA-256-PLUS") {
        return None; // nothing to strip
    }
    let mut body = Vec::new();
    body.extend_from_slice(&10i32.to_be_bytes());
    for m in mechs.iter().filter(|m| **m != b"SCRAM-SHA-256-PLUS") {
        body.extend_from_slice(m);
        body.push(0);
    }
    body.push(0); // list terminator
    let len = (4 + body.len()) as i32; // length field includes itself
    let mut out = Vec::with_capacity(1 + body.len() + 4);
    out.push(b'R');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&body);
    Some(out)
}

/// Auth-type of a backend `'R'` message (the int32 after type+len), if present.
pub fn auth_type(full_msg: &[u8]) -> Option<i32> {
    if full_msg.first() == Some(&b'R') && full_msg.len() >= 9 {
        Some(i32::from_be_bytes([full_msg[5], full_msg[6], full_msg[7], full_msg[8]]))
    } else {
        None
    }
}

/// How a query reads/writes — used for telemetry labels and warming decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    Read,
    Write,
    Ddl,
    Txn,
    Other,
}

impl QueryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            QueryKind::Read => "read",
            QueryKind::Write => "write",
            QueryKind::Ddl => "ddl",
            QueryKind::Txn => "txn",
            QueryKind::Other => "other",
        }
    }
}

/// Classify SQL by its leading keyword (comments/whitespace stripped).
pub fn classify(sql: &str) -> QueryKind {
    let kw: String = first_keyword(strip_leading(sql))
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .collect();
    match kw.as_str() {
        "SELECT" | "WITH" | "TABLE" | "VALUES" | "SHOW" | "EXPLAIN" => QueryKind::Read,
        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "COPY" => QueryKind::Write,
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "GRANT" | "REVOKE" | "COMMENT" | "VACUUM"
        | "ANALYZE" | "REINDEX" | "CLUSTER" => QueryKind::Ddl,
        "BEGIN" | "START" | "COMMIT" | "END" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" | "SET"
        | "RESET" | "DISCARD" => QueryKind::Txn,
        _ => QueryKind::Other,
    }
}

/// Reserved overlay-schema prefix the clone bootstrap uses (gfs_ovl__<schema>).
/// Unqualified reads resolve to the federated overlay when this precedes the
/// faithful schema in search_path.
pub const OVERLAY_PREFIX: &str = "gfs_ovl__";

/// Build a simple-query (`'Q'`) frontend message carrying `sql`.
pub fn build_query_msg(sql: &str) -> Vec<u8> {
    let len = 4 + sql.len() + 1; // length field (incl. itself) + sql + NUL
    let mut out = Vec::with_capacity(1 + len);
    out.push(b'Q');
    out.extend_from_slice(&(len as i32).to_be_bytes());
    out.extend_from_slice(sql.as_bytes());
    out.push(0);
    out
}

/// If `sql` is a `SET [SESSION|LOCAL] search_path {TO|=} <list>`, rewrite the
/// list to interleave the overlay schema before each plain schema, so a client
/// that pins its own search_path still resolves unqualified reads to the
/// federated overlay. Returns the rewritten statement, or `None` when it isn't a
/// SET search_path (the message is then forwarded verbatim).
pub fn rewrite_search_path(sql: &str) -> Option<String> {
    let rest = strip_kw(strip_leading(sql), "set")?;
    // optional SESSION / LOCAL qualifier
    let rest = strip_kw(rest, "session").or_else(|| strip_kw(rest, "local")).unwrap_or(rest);
    let rest = strip_kw(rest, "search_path")?.trim_start();
    let list = if let Some(r) = rest.strip_prefix('=') {
        r
    } else {
        strip_kw(rest, "to")?
    };
    let list = list.trim().trim_end_matches(';').trim();

    let mut items: Vec<String> = Vec::new();
    for raw in list.split(',') {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if is_plain_overlayable(item) {
            items.push(format!("{OVERLAY_PREFIX}{item}"));
        }
        items.push(item.to_string());
    }
    // Dedup preserving order (idempotent if the client already interleaved).
    let mut seen = std::collections::HashSet::new();
    items.retain(|it| seen.insert(it.clone()));
    if items.is_empty() {
        return None;
    }
    Some(format!("SET search_path TO {}", items.join(", ")))
}

/// Case-insensitively strip a leading keyword token (and following whitespace)
/// from `s`, honoring a word boundary. Returns the remainder, or `None`.
fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() >= kw.len() && s[..kw.len()].eq_ignore_ascii_case(kw) {
        let after = &s[kw.len()..];
        match after.chars().next() {
            None => Some(after),
            Some(c) if c.is_whitespace() || c == '=' => Some(after),
            _ => None, // not a word boundary (e.g. "settings")
        }
    } else {
        None
    }
}

/// A schema token we should interleave an overlay before: a plain lowercase
/// identifier, not already an overlay, not a `pg_*` schema, not quoted/special
/// (e.g. `"$user"`).
fn is_plain_overlayable(item: &str) -> bool {
    !item.starts_with(OVERLAY_PREFIX)
        && !item.starts_with("pg_")
        && item
            .bytes()
            .next()
            .is_some_and(|b| b == b'_' || b.is_ascii_lowercase())
        && item.bytes().all(|b| b == b'_' || b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn cstr(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    std::str::from_utf8(&b[..end]).unwrap_or("")
}

fn skip_cstr(b: &[u8]) -> &[u8] {
    let end = b.iter().position(|&c| c == 0).map_or(b.len(), |i| i + 1);
    &b[end..]
}

/// Skip leading whitespace and SQL comments (`-- …`, `/* … */`).
fn strip_leading(mut s: &str) -> &str {
    loop {
        let t = s.trim_start();
        if let Some(rest) = t.strip_prefix("--") {
            s = rest.split_once('\n').map_or("", |(_, r)| r);
        } else if let Some(rest) = t.strip_prefix("/*") {
            s = rest.split_once("*/").map_or("", |(_, r)| r);
        } else {
            return t;
        }
    }
}

fn first_keyword(s: &str) -> &str {
    s.split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .find(|w| !w.is_empty())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_set_search_path() {
        assert_eq!(
            rewrite_search_path("SET search_path = public").unwrap(),
            "SET search_path TO gfs_ovl__public, public"
        );
        assert_eq!(
            rewrite_search_path("SET search_path TO shop, public").unwrap(),
            "SET search_path TO gfs_ovl__shop, shop, gfs_ovl__public, public"
        );
        // SESSION/LOCAL qualifiers, case-insensitive, trailing ';'.
        assert_eq!(
            rewrite_search_path("set SESSION search_path to app;").unwrap(),
            "SET search_path TO gfs_ovl__app, app"
        );
        // Special/quoted tokens and pg_* are left as-is.
        assert_eq!(
            rewrite_search_path(r#"SET search_path = "$user", public, pg_catalog"#).unwrap(),
            r#"SET search_path TO "$user", gfs_ovl__public, public, pg_catalog"#
        );
        // Idempotent: an already-interleaved path is unchanged (deduped).
        assert_eq!(
            rewrite_search_path("SET search_path TO gfs_ovl__shop, shop").unwrap(),
            "SET search_path TO gfs_ovl__shop, shop"
        );
        // Not a SET search_path → no rewrite.
        assert!(rewrite_search_path("SELECT 1").is_none());
        assert!(rewrite_search_path("SET work_mem = '4MB'").is_none());
        assert!(rewrite_search_path("SET search_paths = x").is_none());
    }

    #[test]
    fn build_query_msg_roundtrips() {
        let msg = build_query_msg("SELECT 1");
        assert_eq!(msg[0], b'Q');
        match parse_frontend(&msg) {
            Frontend::Query(q) => assert_eq!(q, "SELECT 1"),
            other => panic!("expected Query, got {other:?}"),
        }
        let (typ, total) = peek_header(&msg).unwrap();
        assert_eq!(typ, b'Q');
        assert_eq!(total, msg.len());
    }

    #[test]
    fn classifies_common_statements() {
        assert_eq!(classify("SELECT 1"), QueryKind::Read);
        assert_eq!(classify("  with x as (select 1) select * from x"), QueryKind::Read);
        assert_eq!(classify("/* c */ insert into t values (1)"), QueryKind::Write);
        assert_eq!(classify("-- hi\nUPDATE t SET a=1"), QueryKind::Write);
        assert_eq!(classify("create table t(id int)"), QueryKind::Ddl);
        assert_eq!(classify("BEGIN"), QueryKind::Txn);
        assert_eq!(classify("FETCH ALL FROM c"), QueryKind::Other);
    }

    #[test]
    fn parses_ssl_request() {
        let mut p = Vec::new();
        p.extend_from_slice(&8i32.to_be_bytes());
        p.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        assert_eq!(parse_startup(&p), Some((StartupKind::SslRequest, 8)));
    }

    #[test]
    fn parses_simple_query_message() {
        let sql = "SELECT 42";
        let mut m = vec![b'Q'];
        let len = 4 + sql.len() + 1;
        m.extend_from_slice(&(len as u32).to_be_bytes());
        m.extend_from_slice(sql.as_bytes());
        m.push(0);
        let (typ, total) = peek_header(&m).unwrap();
        assert_eq!(typ, b'Q');
        assert_eq!(total, m.len());
        match parse_frontend(&m) {
            Frontend::Query(q) => assert_eq!(q, sql),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn parses_extended_parse_message_query() {
        // Parse: stmtName \0 query \0 int16(0)
        let stmt = "s1";
        let sql = "SELECT now()";
        let mut body = Vec::new();
        body.extend_from_slice(stmt.as_bytes());
        body.push(0);
        body.extend_from_slice(sql.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i16.to_be_bytes());
        let mut m = vec![b'P'];
        m.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        m.extend_from_slice(&body);
        match parse_frontend(&m) {
            Frontend::Parse { name, query } => {
                assert_eq!(name, stmt);
                assert_eq!(query, sql);
            }
            other => panic!("expected Parse, got {other:?}"),
        }
    }

    fn bind_msg(portal: &str, stmt: &str, fmts: &[i16], params: &[Option<&[u8]>]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(portal.as_bytes());
        body.push(0);
        body.extend_from_slice(stmt.as_bytes());
        body.push(0);
        body.extend_from_slice(&(fmts.len() as i16).to_be_bytes());
        for f in fmts {
            body.extend_from_slice(&f.to_be_bytes());
        }
        body.extend_from_slice(&(params.len() as i16).to_be_bytes());
        for p in params {
            match p {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(v) => {
                    body.extend_from_slice(&(v.len() as i32).to_be_bytes());
                    body.extend_from_slice(v);
                }
            }
        }
        body.extend_from_slice(&0i16.to_be_bytes()); // 0 result format codes
        let mut m = vec![b'B'];
        m.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        m.extend_from_slice(&body);
        m
    }

    #[test]
    fn parses_bind_text_params() {
        // Two text params (nfmt=0 → all text), one NULL.
        let m = bind_msg("", "s1", &[], &[Some(b"4242"), None]);
        match parse_frontend(&m) {
            Frontend::Bind { stmt, params } => {
                assert_eq!(stmt, "s1");
                assert_eq!(params, Some(vec![Some("4242".to_string()), None]));
            }
            other => panic!("expected Bind, got {other:?}"),
        }
    }

    #[test]
    fn binds_with_binary_params_are_skipped() {
        // One binary-format param (format code 1) → params None.
        let m = bind_msg("", "s1", &[1], &[Some(&[0, 0, 16, 146])]);
        match parse_frontend(&m) {
            Frontend::Bind { params, .. } => assert_eq!(params, None),
            other => panic!("expected Bind, got {other:?}"),
        }
    }

    fn sasl_msg(mechs: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&10i32.to_be_bytes());
        for m in mechs {
            body.extend_from_slice(m.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut m = vec![b'R'];
        m.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        m.extend_from_slice(&body);
        m
    }

    #[test]
    fn strips_scram_plus_mechanism() {
        let msg = sasl_msg(&["SCRAM-SHA-256-PLUS", "SCRAM-SHA-256"]);
        let out = strip_scram_plus(&msg).expect("should rewrite");
        // Rewritten message parses as a single SCRAM-SHA-256 mechanism.
        let want = sasl_msg(&["SCRAM-SHA-256"]);
        assert_eq!(out, want);
        assert_eq!(auth_type(&out), Some(10));
        // No PLUS, and no rewrite when only plain SCRAM is offered.
        assert!(strip_scram_plus(&want).is_none());
    }

    #[test]
    fn substitutes_text_params() {
        let q = "SELECT * FROM t WHERE id = $1 AND name = $2 AND z IS NOT $3";
        let got = substitute_params(
            q,
            &[Some("42".into()), Some("o'brien".into()), None],
        );
        assert_eq!(
            got,
            "SELECT * FROM t WHERE id = '42' AND name = 'o''brien' AND z IS NOT NULL"
        );
    }
}
