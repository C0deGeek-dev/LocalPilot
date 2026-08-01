//! A tiny echo/ping serve loop over the transport, plus the framed-record
//! writer both it and the tests use.
//!
//! This is not session hosting — it exists to prove the transport end to end.
//! Reads reuse `localpilot-rpc`'s [`JsonRecordReader`](localpilot_rpc::JsonRecordReader)
//! (LF-delimited NDJSON, 16 MiB record cap); writes serialize a
//! [`serde_json::Value`] and terminate it with the same single LF.

use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use localpilot_rpc::JsonRecordReader;

use crate::transport::Conn;

/// Serve one connection with an echo/ping loop until the peer closes.
///
/// Each inbound JSON record is answered: an object carrying a `ping` field gets
/// a `{"pong": <payload>}` reply; any other record is echoed back verbatim.
///
/// # Errors
/// [`std::io::Error`] on a transport read/write failure.
pub async fn serve_echo(conn: Conn) -> std::io::Result<()> {
    let (read, mut write) = tokio::io::split(conn);
    let mut reader = JsonRecordReader::new(read);
    while let Some(value) = reader.next().await? {
        // `.cloned()` takes an owned payload so the `None` arm can move `value`.
        let reply = match value.get("ping").cloned() {
            Some(payload) => serde_json::json!({ "pong": payload }),
            None => value,
        };
        write_record(&mut write, &reply).await?;
    }
    Ok(())
}

/// Write one JSON record framed as LF-terminated NDJSON, then flush.
///
/// # Errors
/// [`std::io::Error`] if serialization or the underlying write fails.
pub(crate) async fn write_record<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(())
}
