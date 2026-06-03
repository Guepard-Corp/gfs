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

pub fn peek_header(buf: &[u8]) -> Option<(u8, usize)> {
    if buf.len() < 5 {
        return None;
    }
    let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize; // excludes type byte
    Some((buf[0], 1 + len))
}

/// Parse one complete typed frontend message at the front of `buf`.
/// Caller must ensure `buf` holds at least `peek_header().1` bytes.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssl_request() {
        let mut p = Vec::new();
        p.extend_from_slice(&8i32.to_be_bytes());
        p.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        assert_eq!(parse_startup(&p), Some((StartupKind::SslRequest, 8)));
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

}
