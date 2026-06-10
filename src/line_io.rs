//! Bounded line reads from external, newline-delimited IPC streams.
//!
//! caucus reads newline-delimited JSON from several external peers: the
//! turn-signal socket (any local process the hook runs as), the control socket
//! (the MCP shim), and the JSON-RPC stdin of `caucus mcp serve`. A plain
//! `read_line` / `lines().next_line()` grows its buffer until a newline
//! arrives, so a peer that sends a stream with no newline — buggy or hostile —
//! makes the reader allocate without bound until it is killed. One source of
//! truth for "read a line, but never more than a fixed cap" closes that for
//! every such reader at once (Invariant: no external stream read may allocate
//! past [`MAX_IPC_LINE_BYTES`]).

use tokio::io::{AsyncBufRead, AsyncReadExt};

/// Hard cap on a single inbound IPC line. Generous next to any legitimate
/// payload (a turn signal, a control request, a JSON-RPC call are all well
/// under this) yet small enough that a newline-less flood cannot exhaust
/// memory: a line at or past this is rejected, not buffered further.
pub(crate) const MAX_IPC_LINE_BYTES: u64 = 1 << 20; // 1 MiB

/// Outcome of [`read_capped_line`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CappedLine {
    /// A complete line was read into the caller's buffer, trailing `\n` (and a
    /// preceding `\r`) stripped — matching `AsyncBufReadExt::next_line`.
    Line,
    /// Clean end of stream before any line — the peer closed.
    Eof,
    /// The line reached the byte cap without a terminating newline. The stream
    /// is now desynchronised (the read stopped mid-line), so the caller must
    /// stop reading this connection rather than resume into the leftover bytes.
    TooLong,
}

/// Read one newline-delimited line from `reader` into `buf`, reading at most
/// `max` bytes so a peer cannot force an unbounded allocation.
///
/// `buf` is cleared first. A line of exactly `max` bytes including its newline
/// still completes; anything longer trips [`CappedLine::TooLong`]. A non-UTF-8
/// or partial-UTF-8 read surfaces as the underlying `io::Error`.
pub(crate) async fn read_capped_line<R>(
    reader: &mut R,
    buf: &mut String,
    max: u64,
) -> std::io::Result<CappedLine>
where
    R: AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;

    buf.clear();
    // `take` bounds the bytes this read may consume; `read_line` then stops at
    // the first newline, the cap, or EOF — whichever comes first.
    let mut limited = (&mut *reader).take(max);
    let n = limited.read_line(buf).await?;
    if n == 0 {
        return Ok(CappedLine::Eof);
    }
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
        return Ok(CappedLine::Line);
    }
    // No newline. Either the cap was hit mid-line (n == max → reject), or the
    // peer closed after an unterminated final chunk (n < max → treat as EOF: a
    // newline-delimited protocol has no complete message there either way).
    if n as u64 >= max {
        Ok(CappedLine::TooLong)
    } else {
        Ok(CappedLine::Eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    /// A normal line returns `Line` with the newline stripped, and a second
    /// read off the same buffered reader returns the next line — the cap is
    /// per-line, not per-connection.
    #[tokio::test]
    async fn reads_successive_lines_stripped() {
        let mut reader = BufReader::new(&b"first\nsecond\n"[..]);
        let mut buf = String::new();

        assert_eq!(
            read_capped_line(&mut reader, &mut buf, MAX_IPC_LINE_BYTES)
                .await
                .unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, "first");
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, MAX_IPC_LINE_BYTES)
                .await
                .unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, "second");
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, MAX_IPC_LINE_BYTES)
                .await
                .unwrap(),
            CappedLine::Eof
        );
    }

    /// A trailing `\r\n` is stripped to match `next_line`.
    #[tokio::test]
    async fn strips_crlf() {
        let mut reader = BufReader::new(&b"hello\r\n"[..]);
        let mut buf = String::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, MAX_IPC_LINE_BYTES)
                .await
                .unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, "hello");
    }

    /// A line longer than the cap, with no newline in range, is rejected as
    /// `TooLong` instead of being buffered without bound.
    #[tokio::test]
    async fn rejects_a_line_past_the_cap() {
        let flood = [b'x'; 64];
        let mut reader = BufReader::new(&flood[..]);
        let mut buf = String::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 16).await.unwrap(),
            CappedLine::TooLong
        );
        assert!(
            buf.len() <= 16,
            "the buffer never grows past the cap: {}",
            buf.len()
        );
    }

    /// A line of exactly `max` bytes *including* its newline still completes —
    /// the cap counts the terminator, so a maximal legitimate line is not
    /// spuriously rejected.
    #[tokio::test]
    async fn a_line_filling_the_cap_with_its_newline_completes() {
        let mut reader = BufReader::new(&b"abc\n"[..]); // 4 bytes incl. '\n'
        let mut buf = String::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, 4).await.unwrap(),
            CappedLine::Line
        );
        assert_eq!(buf, "abc");
    }

    /// Unterminated trailing bytes before EOF (shorter than the cap) are not a
    /// complete line: reported as `Eof`, not `Line`.
    #[tokio::test]
    async fn unterminated_short_tail_is_eof() {
        let mut reader = BufReader::new(&b"partial"[..]);
        let mut buf = String::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut buf, MAX_IPC_LINE_BYTES)
                .await
                .unwrap(),
            CappedLine::Eof
        );
    }
}
