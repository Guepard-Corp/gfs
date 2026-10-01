//! An `AsyncRead` that drops blank lines before the JSON-RPC parser sees them.
//!
//! A single empty line on stdin terminated the server with exit code 0. So did
//! a line of non-JSON. rmcp reports both as `QuitReason::Closed`, the same
//! value a clean client disconnect produces, so nothing downstream could tell
//! them apart: a supervisor saw a successful shutdown, and any in-flight
//! request was lost with no error.
//!
//! Only the blank-line case can be fixed without second-guessing the protocol,
//! and it is the one that happens by accident — a client that flushes a stray
//! newline, a shell pipeline that appends one, a human testing by hand. A line
//! with actual content that is not valid JSON is a genuine protocol violation
//! and is left to rmcp.
//!
//! The filter is line-oriented because the transport is: messages are
//! newline-delimited, so dropping a line that holds nothing but whitespace
//! cannot split or corrupt a message. It never buffers more than one line.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncBufRead, AsyncRead, BufReader, ReadBuf};

/// Wraps a byte stream and forwards every line except the blank ones.
pub struct SkipBlankLines<R> {
    inner: BufReader<R>,
    /// The line being accumulated. Not yet judged, because a line cannot be
    /// judged blank until its terminator has arrived.
    line: Vec<u8>,
    /// A line that has been judged and is being handed to the caller, and how
    /// much of it has gone already.
    pending: Vec<u8>,
    offset: usize,
    done: bool,
}

impl<R: AsyncRead + Unpin> SkipBlankLines<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
            line: Vec::new(),
            pending: Vec::new(),
            offset: 0,
            done: false,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for SkipBlankLines<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            // Hand over whatever of the judged line is left.
            if this.offset < this.pending.len() {
                let n = std::cmp::min(buf.remaining(), this.pending.len() - this.offset);
                buf.put_slice(&this.pending[this.offset..this.offset + n]);
                this.offset += n;
                return Poll::Ready(Ok(()));
            }
            // That one is spent.
            this.pending.clear();
            this.offset = 0;
            if this.done {
                return Poll::Ready(Ok(()));
            }

            // Accumulate until the line is terminated, and only then judge it.
            //
            // Nothing is forwarded from a line before its terminator arrives.
            // Forwarding early is what broke this: `writeln!` emits the content
            // and the `\n` as separate writes, a pipe can deliver them as
            // separate reads, and the lone `\n` then looks like a complete
            // all-whitespace line. Dropping it stripped the terminator from a
            // message already handed over, so the JSON-RPC parser never
            // dispatched that message and glued the next one onto it.
            match Pin::new(&mut this.inner).poll_fill_buf(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(available)) => {
                    if available.is_empty() {
                        this.done = true;
                        if this.line.is_empty() {
                            return Poll::Ready(Ok(()));
                        }
                        // A final line with no terminator is never judged blank,
                        // and must not be lost.
                        std::mem::swap(&mut this.pending, &mut this.line);
                        this.line.clear();
                        continue;
                    }
                    let take = match available.iter().position(|b| *b == b'\n') {
                        Some(at) => at + 1,
                        None => available.len(),
                    };
                    this.line.extend_from_slice(&available[..take]);
                    Pin::new(&mut this.inner).consume(take);

                    if this.line.last() != Some(&b'\n') {
                        continue; // still partial -- keep reading
                    }
                    if this.line.iter().all(|b| b.is_ascii_whitespace()) {
                        this.line.clear(); // a blank line: drop it
                        continue;
                    }
                    std::mem::swap(&mut this.pending, &mut this.line);
                    this.line.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn filtered(input: &str) -> String {
        let mut out = String::new();
        SkipBlankLines::new(input.as_bytes())
            .read_to_string(&mut out)
            .await
            .expect("read");
        out
    }

    #[tokio::test]
    async fn blank_lines_are_dropped_and_messages_are_not() {
        assert_eq!(filtered("{\"a\":1}\n").await, "{\"a\":1}\n");
        assert_eq!(filtered("\n{\"a\":1}\n").await, "{\"a\":1}\n");
        assert_eq!(
            filtered("{\"a\":1}\n\n\n{\"b\":2}\n").await,
            "{\"a\":1}\n{\"b\":2}\n"
        );
        assert_eq!(filtered("\n\n\n").await, "");
        // Whitespace-only counts as blank; whitespace around content does not.
        assert_eq!(filtered("   \t \n{\"a\":1}\n").await, "{\"a\":1}\n");
        assert_eq!(filtered("  {\"a\":1}  \n").await, "  {\"a\":1}  \n");
    }

    /// A line with content that is not JSON is a protocol violation, not an
    /// accident, and must still reach rmcp rather than being swallowed here.
    #[tokio::test]
    async fn non_json_content_is_passed_through() {
        assert_eq!(filtered("garbage\n").await, "garbage\n");
    }

    /// The last line may have no terminator; it must not be judged blank on the
    /// strength of an incomplete read, and must not be lost.
    #[tokio::test]
    async fn a_final_line_without_a_newline_survives() {
        assert_eq!(filtered("{\"a\":1}").await, "{\"a\":1}");
        assert_eq!(
            filtered("{\"a\":1}\n{\"b\":2}").await,
            "{\"a\":1}\n{\"b\":2}"
        );
    }

    /// Feeds the filter one chunk per `poll_read`, so a line can be split across
    /// reads the way a pipe actually delivers it.
    struct Chunks(std::collections::VecDeque<Vec<u8>>);

    impl Chunks {
        fn new(parts: &[&str]) -> Self {
            Self(parts.iter().map(|s| s.as_bytes().to_vec()).collect())
        }
    }

    impl AsyncRead for Chunks {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let Some(mut chunk) = self.0.pop_front() else {
                return Poll::Ready(Ok(())); // EOF
            };
            let n = std::cmp::min(buf.remaining(), chunk.len());
            buf.put_slice(&chunk[..n]);
            if n < chunk.len() {
                self.0.push_front(chunk.split_off(n));
            }
            Poll::Ready(Ok(()))
        }
    }

    async fn filtered_chunks(parts: &[&str]) -> String {
        let mut out = String::new();
        SkipBlankLines::new(Chunks::new(parts))
            .read_to_string(&mut out)
            .await
            .expect("read");
        out
    }

    /// A terminator that arrives in its own read must not be mistaken for a
    /// blank line.
    ///
    /// `writeln!` emits the content and the `\n` as separate writes, so a pipe
    /// can deliver them as separate reads. The content is forwarded first; the
    /// lone `\n` then arrives as a "complete, all-whitespace line" and was
    /// dropped -- leaving the message with no terminator, so the JSON-RPC parser
    /// never dispatched it and glued the next message onto it.
    #[tokio::test]
    async fn a_terminator_in_its_own_read_is_not_a_blank_line() {
        assert_eq!(filtered_chunks(&["{\"a\":1}", "\n"]).await, "{\"a\":1}\n");
        assert_eq!(
            filtered_chunks(&["{\"a\":1}", "\n", "{\"b\":2}", "\n"]).await,
            "{\"a\":1}\n{\"b\":2}\n"
        );
        // A genuinely blank line delivered on its own is still dropped.
        assert_eq!(
            filtered_chunks(&["{\"a\":1}\n", "\n", "{\"b\":2}\n"]).await,
            "{\"a\":1}\n{\"b\":2}\n"
        );
        // And one split mid-content, with its terminator separate again.
        assert_eq!(
            filtered_chunks(&["{\"a\"", ":1}", "\n"]).await,
            "{\"a\":1}\n"
        );
        // A blank line split across reads is still blank. Judging only whole
        // lines gets this for free; a flag saying "we are mid-line" would not,
        // and would leak the whitespace through to rmcp as a protocol
        // violation -- the failure this filter exists to prevent.
        assert_eq!(
            filtered_chunks(&["{\"a\":1}\n", "  ", " \n", "{\"b\":2}\n"]).await,
            "{\"a\":1}\n{\"b\":2}\n"
        );
        // Several messages arriving in one read still split correctly.
        assert_eq!(
            filtered_chunks(&["{\"a\":1}\n\n{\"b\":2}\n"]).await,
            "{\"a\":1}\n{\"b\":2}\n"
        );
    }

    /// Reading through a tiny buffer must not change what comes out.
    #[tokio::test]
    async fn output_does_not_depend_on_read_size() {
        let input = "\n{\"a\":1}\n\n{\"b\":2}\n\n";
        let mut reader = SkipBlankLines::new(input.as_bytes());
        let mut out = Vec::new();
        let mut one = [0u8; 1];
        loop {
            let n = reader.read(&mut one).await.expect("read");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&one[..n]);
        }
        assert_eq!(String::from_utf8(out).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
    }
}
