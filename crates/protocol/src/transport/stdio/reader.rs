//! Bounded line reader for the stdio transport — the OOM guard (L-2).
//!
//! Extracted from `stdio.rs` to keep that file under the 300-line production
//! convention. Declared as a private child module of `stdio` via
//! `#[path = "stdio_line_reader.rs"] mod stdio_line_reader;` inside `stdio.rs`
//! so `transport/mod.rs` (owned by a different workstream) needs no new
//! `pub mod` line — same technique as `stderr_pump`. See ADR 0013.

use tokio::io::AsyncBufReadExt;

use super::MAX_LINE_BYTES;
use crate::transport::TransportError;

/// Read one newline-terminated line from `reader` into `out`, but abort once
/// `MAX_LINE_BYTES` would be exceeded. Returns the number of bytes pushed
/// (including the trailing `\n` if any). Returns `0` on EOF with nothing read.
///
/// This is the OOM guard for L-2: a malicious server-under-test can emit one
/// gigantic line; the default `read_line` would happily buffer it all. We use
/// `fill_buf`/`consume` so we can stop reading the moment we'd cross the cap.
pub(super) async fn read_bounded_line<R>(
    reader: &mut R,
    out: &mut String,
) -> Result<usize, TransportError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut total = 0usize;
    loop {
        // Scope the `&[u8]` borrow of `reader` so we can call `consume`
        // afterwards. We extract everything we need from `buf` into owned
        // values before releasing the borrow.
        let (chunk, found_newline) = {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                // EOF.
                return Ok(total);
            }
            match buf.iter().position(|&b| b == b'\n') {
                Some(pos) => (buf[..=pos].to_vec(), true),
                None => (buf.to_vec(), false),
            }
        };
        if total + chunk.len() > MAX_LINE_BYTES {
            return Err(TransportError::Other(format!(
                "stdio transport: response line exceeds {MAX_LINE_BYTES} bytes; aborting to avoid OOM"
            )));
        }
        push_utf8_lossy(out, &chunk);
        let n = chunk.len();
        reader.consume(n);
        total += n;
        if found_newline {
            return Ok(total);
        }
    }
}

/// Append `bytes` to `out`, replacing invalid UTF-8 with U+FFFD. Mirrors what
/// `BufReader::read_line` would surface as an `InvalidData` error, but we'd
/// rather tolerate junk and let the JSON parser reject the frame than abort
/// the whole transport on a single bad byte.
fn push_utf8_lossy(out: &mut String, bytes: &[u8]) {
    out.push_str(&String::from_utf8_lossy(bytes));
}
