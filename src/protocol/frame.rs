use super::codec::{invalid, put_varint};
use std::io::{self, ErrorKind};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME_SIZE: usize = 2_097_151;

/// None means clean EOF at a packet boundary; partial frames are errors.
/// Reading exact lengths handles fragmented and coalesced TCP packets without over-reading.
pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut length = 0usize;
    for i in 0..3 {
        let mut byte = [0u8];
        let count = stream.read(&mut byte).await?;
        if count == 0 {
            if i == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "truncated frame length",
            ));
        }
        length |= usize::from(byte[0] & 0x7f) << (7 * i);
        if byte[0] & 0x80 == 0 {
            if length == 0 || length > MAX_FRAME_SIZE {
                return Err(invalid("invalid frame size"));
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await?;
            return Ok(Some(body));
        }
    }
    Err(invalid("frame length exceeds three bytes"))
}

pub async fn write_frame<W: AsyncWrite + Unpin>(stream: &mut W, body: &[u8]) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_FRAME_SIZE {
        return Err(invalid("invalid frame size"));
    }
    let mut prefix = Vec::with_capacity(3);
    put_varint(body.len() as i32, &mut prefix);
    stream.write_all(&prefix).await?;
    stream.write_all(body).await?;
    stream.flush().await
}
