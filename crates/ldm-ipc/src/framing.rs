//! Message framing helpers.
//!
//! * Native Messaging: little-endian u32 length followed by UTF-8 JSON.
//! * Unix-socket IPC: newline-delimited JSON (NDJSON) – one envelope per line.
use std::io::{self, Read, Write};

use crate::MAX_MESSAGE_BYTES;

/// Read one native-messaging frame. Returns `Ok(None)` on clean EOF.
pub fn read_native<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message too large: {n} bytes")));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Write one native-messaging frame and flush.
pub fn write_native<W: Write>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "message too large"));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()
}

#[cfg(feature = "async")]
pub mod asyncio {
    use super::*;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    pub async fn read_native<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
        let mut len = [0u8; 4];
        match r.read_exact(&mut len).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = u32::from_le_bytes(len) as usize;
        if n > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
        }
        let mut buf = vec![0u8; n];
        r.read_exact(&mut buf).await?;
        Ok(Some(buf))
    }

    pub async fn write_native<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
        w.write_all(&(bytes.len() as u32).to_le_bytes()).await?;
        w.write_all(bytes).await?;
        w.flush().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_roundtrip() {
        let mut buf = Vec::new();
        write_native(&mut buf, br#"{"a":1}"#).unwrap();
        assert_eq!(&buf[..4], &7u32.to_le_bytes());
        let mut cur = io::Cursor::new(buf);
        assert_eq!(read_native(&mut cur).unwrap().unwrap(), br#"{"a":1}"#);
        assert!(read_native(&mut cur).unwrap().is_none());
    }

    #[test]
    fn native_rejects_oversize() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_MESSAGE_BYTES as u32) + 1).to_le_bytes());
        let mut cur = io::Cursor::new(buf);
        assert!(read_native(&mut cur).is_err());
    }
}
