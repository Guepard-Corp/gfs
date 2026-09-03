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
    /// The current line, still to be handed to the caller, and how much of it
    /// has been handed over already.
    pending: Vec<u8>,
    offset: usize,
    done: bool,
}

impl<R: AsyncRead + Unpin> SkipBlankLines<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner: BufReader::new(inner),
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
            // Hand over whatever of the current line is left.
            if this.offset < this.pending.len() {
                let n = std::cmp::min(buf.remaining(), this.pending.len() - this.offset);
                buf.put_slice(&this.pending[this.offset..this.offset + n]);
                this.offset += n;
                return Poll::Ready(Ok(()));
            }
            if this.done {
                return Poll::Ready(Ok(()));
            }

            // Pull the next line, skipping any that hold only whitespace.
            this.pending.clear();
            this.offset = 0;
            match Pin::new(&mut this.inner).poll_fill_buf(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(available)) => {
                    if available.is_empty() {
                        this.done = true;
                        return Poll::Ready(Ok(()));
                    }
                    let take = match available.iter().position(|b| *b == b'\n') {
                        Some(at) => at + 1,
                        // No newline yet: take what there is and come back for
                        // the rest. A partial line is never judged blank.
                        None => available.len(),
                    };
                    this.pending.extend_from_slice(&available[..take]);
                    Pin::new(&mut this.inner).consume(take);

                    let complete = this.pending.last() == Some(&b'\n');
                    if complete && this.pending.iter().all(|b| b.is_ascii_whitespace()) {
                        // A blank line. Drop it and look at the next one.
                        this.pending.clear();
                    }
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
