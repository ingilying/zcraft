//! Stateful framing with optional zlib compression and AES-CFB8 encryption.
//! Reads are cancellation-safe and never cross a frame boundary.
use crate::protocol::{MAX_FRAME_SIZE, Reader, invalid, put_varint, write_frame};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::ZlibEncoder};
use openssl::symm::{Cipher, Crypter, Mode};
use std::io::{self, Write};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::Instant,
};

pub struct Transport<S> {
    stream: S,
    ciphers: Option<(Crypter, Crypter)>,
    compression_threshold: Option<usize>,
    read_buffer: Vec<u8>,
    read_started: Option<Instant>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            ciphers: None,
            compression_threshold: None,
            read_buffer: Vec::new(),
            read_started: None,
        }
    }

    pub fn enable_encryption(&mut self, secret: &[u8; 16]) -> io::Result<()> {
        if self.ciphers.is_some() || !self.read_buffer.is_empty() {
            return Err(invalid(
                "encryption must be enabled once at a frame boundary",
            ));
        }
        let cipher = Cipher::aes_128_cfb8();
        let encrypt =
            Crypter::new(cipher, Mode::Encrypt, secret, Some(secret)).map_err(io::Error::other)?;
        let decrypt =
            Crypter::new(cipher, Mode::Decrypt, secret, Some(secret)).map_err(io::Error::other)?;
        self.ciphers = Some((encrypt, decrypt));
        Ok(())
    }

    /// Switch both directions to compression framing at a packet boundary.
    pub fn enable_compression(&mut self, threshold: usize) -> io::Result<()> {
        if self.compression_threshold.is_some() || !self.read_buffer.is_empty() {
            return Err(invalid(
                "compression must be enabled once at a frame boundary",
            ));
        }
        if threshold > MAX_FRAME_SIZE {
            return Err(invalid("compression threshold exceeds frame limit"));
        }
        self.compression_threshold = Some(threshold);
        Ok(())
    }

    pub fn partial_frame_started(&self) -> Option<Instant> {
        self.read_started
    }

    fn frame_length(&self) -> io::Result<Option<(usize, usize)>> {
        let mut length = 0usize;
        for (index, byte) in self.read_buffer.iter().take(3).enumerate() {
            length |= usize::from(byte & 0x7f) << (index * 7);
            if byte & 0x80 == 0 {
                if length == 0 || length > MAX_FRAME_SIZE {
                    return Err(invalid("invalid frame size"));
                }
                return Ok(Some((index + 1, length)));
            }
        }
        if self.read_buffer.len() >= 3 {
            return Err(invalid("frame length exceeds three bytes"));
        }
        Ok(None)
    }

    pub async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let needed = match self.frame_length()? {
                Some((prefix, length)) => {
                    if self.read_buffer.len() == prefix + length {
                        let mut frame = std::mem::take(&mut self.read_buffer);
                        frame.drain(..prefix);
                        self.read_started = None;
                        if let Some(threshold) = self.compression_threshold {
                            frame = decode_compression(frame, threshold)?;
                        }
                        return Ok(Some(frame));
                    }
                    prefix + length - self.read_buffer.len()
                }
                None => 1,
            };
            let mut chunk = [0u8; 8192];
            let limit = needed.min(chunk.len());
            // AsyncReadExt::read is cancellation-safe. Once it succeeds, store
            // bytes and advance the cipher before the next possible await.
            let count = self.stream.read(&mut chunk[..limit]).await?;
            if count == 0 {
                if self.read_buffer.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame",
                ));
            }
            self.read_started.get_or_insert_with(Instant::now);
            if let Some((_, decrypt)) = &mut self.ciphers {
                self.read_buffer
                    .extend(transform(decrypt, &chunk[..count])?);
            } else {
                self.read_buffer.extend_from_slice(&chunk[..count]);
            }
        }
    }

    pub async fn write_frame(&mut self, body: &[u8]) -> io::Result<()> {
        if body.is_empty() || body.len() > MAX_FRAME_SIZE {
            return Err(invalid("invalid frame size"));
        }
        let compressed;
        let body = if let Some(threshold) = self.compression_threshold {
            compressed = encode_compression(body, threshold)?;
            compressed.as_slice()
        } else {
            body
        };
        let Some((encrypt, _)) = &mut self.ciphers else {
            return write_frame(&mut self.stream, body).await;
        };
        let mut frame = Vec::with_capacity(body.len() + 3);
        put_varint(body.len() as i32, &mut frame);
        frame.extend_from_slice(body);
        let encrypted = transform(encrypt, &frame)?;
        self.stream.write_all(&encrypted).await?;
        self.stream.flush().await
    }
}

fn encode_compression(body: &[u8], threshold: usize) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    if body.len() < threshold {
        frame.push(0);
        frame.extend_from_slice(body);
    } else {
        put_varint(body.len() as i32, &mut frame);
        let mut encoder = ZlibEncoder::new(frame, Compression::fast());
        encoder.write_all(body)?;
        frame = encoder.finish()?;
    }
    if frame.len() > MAX_FRAME_SIZE {
        return Err(invalid("compressed framing exceeds frame limit"));
    }
    Ok(frame)
}

fn decode_compression(mut frame: Vec<u8>, threshold: usize) -> io::Result<Vec<u8>> {
    let mut reader = Reader::new(&frame);
    let declared = reader.varint()?;
    let payload = reader.remainder();
    let offset = frame.len() - payload.len();
    if declared == 0 {
        if payload.is_empty() || payload.len() >= threshold {
            return Err(invalid("invalid uncompressed packet length"));
        }
        frame.drain(..offset);
        return Ok(frame);
    }
    let declared =
        usize::try_from(declared).map_err(|_| invalid("negative decompressed length"))?;
    if declared < threshold || declared > MAX_FRAME_SIZE {
        return Err(invalid("decompressed length outside allowed range"));
    }
    // A spare byte detects output beyond the declaration and lets the decoder
    // consume the checksum even when the valid output is exactly declared bytes.
    let mut out = vec![0; declared + 1];
    let mut decoder = Decompress::new(true);
    let status = decoder
        .decompress(payload, &mut out, FlushDecompress::Finish)
        .map_err(|_| invalid("invalid zlib stream"))?;
    if status != Status::StreamEnd
        || decoder.total_out() != declared as u64
        || decoder.total_in() != payload.len() as u64
    {
        return Err(invalid("zlib stream does not match frame lengths"));
    }
    out.truncate(declared);
    Ok(out)
}

fn transform(cipher: &mut Crypter, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = vec![0; input.len() + 16];
    let count = cipher
        .update(input, &mut output)
        .map_err(io::Error::other)?;
    if count != input.len() {
        return Err(invalid("unexpected CFB8 output length"));
    }
    output.truncate(count);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn compression_threshold_and_frame_bounds() {
        for (threshold, size) in [
            (256, 255),
            (256, 256),
            (256, 257),
            (0, 1),
            (0, MAX_FRAME_SIZE),
        ] {
            let body = vec![42; size];
            let encoded = encode_compression(&body, threshold).unwrap();
            assert_eq!(
                Reader::new(&encoded).varint().unwrap(),
                if size < threshold { 0 } else { size as i32 }
            );
            assert_eq!(decode_compression(encoded, threshold).unwrap(), body);
        }
    }

    #[test]
    fn rejects_invalid_compression_lengths_and_zlib_streams() {
        let valid = encode_compression(&[42; 32], 0).unwrap();
        let mut understated = valid.clone();
        understated[0] = 31;
        let mut overstated = valid.clone();
        overstated[0] = 33;
        let mut trailing = valid.clone();
        trailing.extend_from_slice(&valid[1..]);
        let mut corrupt = valid.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        let mut negative = Vec::new();
        put_varint(-1, &mut negative);
        let mut oversized = Vec::new();
        put_varint(MAX_FRAME_SIZE as i32 + 1, &mut oversized);
        let mut bomb = vec![1];
        let large = encode_compression(&vec![0; MAX_FRAME_SIZE], 0).unwrap();
        let mut reader = Reader::new(&large);
        assert_eq!(reader.varint().unwrap(), MAX_FRAME_SIZE as i32);
        bomb.extend_from_slice(reader.remainder());
        for packet in [
            vec![0],
            vec![0, 7],
            vec![0x80],
            vec![5, 0, 0],
            understated,
            overstated,
            trailing,
            corrupt,
            negative,
            oversized,
            bomb,
            valid[..valid.len() - 1].to_vec(),
        ] {
            assert!(decode_compression(packet, 0).is_err());
        }
        assert!(decode_compression(valid, 33).is_err());
        assert!(decode_compression(vec![0, 7], 1).is_err());
    }

    #[tokio::test]
    async fn cancelled_compressed_read_preserves_cipher_state_and_next_frame() {
        let (mut sender, receiver) = tokio::io::duplex(1024);
        let mut client = Transport::new(receiver);
        let key = [9; 16];
        client.enable_encryption(&key).unwrap();
        client.enable_compression(16).unwrap();
        let mut wire = Vec::new();
        for body in [&[22; 512][..], &[1, 2, 3]] {
            let frame = encode_compression(body, 16).unwrap();
            put_varint(frame.len() as i32, &mut wire);
            wire.extend_from_slice(&frame);
        }
        let wire = openssl::symm::encrypt(Cipher::aes_128_cfb8(), &key, Some(&key), &wire).unwrap();
        sender.write_all(&wire[..4]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), client.read_frame())
                .await
                .is_err()
        );
        assert!(client.partial_frame_started().is_some());
        sender.write_all(&wire[4..]).await.unwrap();
        assert_eq!(client.read_frame().await.unwrap().unwrap(), [22; 512]);
        assert_eq!(client.read_frame().await.unwrap().unwrap(), [1, 2, 3]);
        assert!(client.partial_frame_started().is_none());
    }

    #[tokio::test]
    async fn cancelled_read_preserves_partial_plaintext_and_cipher_state() {
        for encrypted in [false, true] {
            let (mut sender, receiver) = tokio::io::duplex(64);
            let mut transport = Transport::new(receiver);
            let secret = [7; 16];
            let mut wire = vec![4, 0, 1, 2, 3, 2, 4, 5];
            if encrypted {
                transport.enable_encryption(&secret).unwrap();
                wire =
                    openssl::symm::encrypt(Cipher::aes_128_cfb8(), &secret, Some(&secret), &wire)
                        .unwrap();
            }
            sender.write_all(&wire[..2]).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(10), transport.read_frame())
                    .await
                    .is_err()
            );
            let started = transport.partial_frame_started().unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(10), transport.read_frame())
                    .await
                    .is_err()
            );
            assert_eq!(transport.partial_frame_started(), Some(started));
            sender.write_all(&wire[2..]).await.unwrap();
            assert_eq!(transport.read_frame().await.unwrap().unwrap(), [0, 1, 2, 3]);
            assert_eq!(transport.partial_frame_started(), None);
            assert_eq!(transport.read_frame().await.unwrap().unwrap(), [4, 5]);
        }
    }
}
