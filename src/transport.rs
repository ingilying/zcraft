//! Stateful plaintext/AES-CFB8 framing. Reads are cancellation-safe so a timer
//! can interrupt polling without dropping bytes or resetting cipher state.
//! Never reads beyond a frame boundary, including the encryption switch.
use crate::protocol::{MAX_FRAME_SIZE, invalid, put_varint, write_frame};
use openssl::symm::{Cipher, Crypter, Mode};
use std::io;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::Instant,
};

pub struct Transport<S> {
    stream: S,
    ciphers: Option<(Crypter, Crypter)>,
    read_buffer: Vec<u8>,
    read_started: Option<Instant>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            ciphers: None,
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
        let Some((encrypt, _)) = &mut self.ciphers else {
            return write_frame(&mut self.stream, body).await;
        };
        if body.is_empty() || body.len() > MAX_FRAME_SIZE {
            return Err(invalid("invalid frame size"));
        }
        let mut frame = Vec::with_capacity(body.len() + 3);
        put_varint(body.len() as i32, &mut frame);
        frame.extend_from_slice(body);
        let encrypted = transform(encrypt, &frame)?;
        self.stream.write_all(&encrypted).await?;
        self.stream.flush().await
    }
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
