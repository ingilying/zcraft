//! Stateful AES-128-CFB8 over the entire Minecraft stream, including length prefixes.
//! No read-ahead: encryption can switch immediately after Encryption Response.
use crate::protocol::{MAX_FRAME_SIZE, invalid, put_varint, read_frame, write_frame};
use openssl::symm::{Cipher, Crypter, Mode};
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct Transport<S> {
    stream: S,
    ciphers: Option<(Crypter, Crypter)>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Transport<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            ciphers: None,
        }
    }

    pub fn enable_encryption(&mut self, secret: &[u8; 16]) -> io::Result<()> {
        if self.ciphers.is_some() {
            return Err(invalid("encryption already enabled"));
        }
        let cipher = Cipher::aes_128_cfb8();
        let encrypt =
            Crypter::new(cipher, Mode::Encrypt, secret, Some(secret)).map_err(io::Error::other)?;
        let decrypt =
            Crypter::new(cipher, Mode::Decrypt, secret, Some(secret)).map_err(io::Error::other)?;
        self.ciphers = Some((encrypt, decrypt));
        Ok(())
    }

    pub async fn read_frame(&mut self) -> io::Result<Option<Vec<u8>>> {
        let Some((_, decrypt)) = &mut self.ciphers else {
            return read_frame(&mut self.stream).await;
        };
        let mut length = 0usize;
        for i in 0..3 {
            let mut byte = [0u8];
            if self.stream.read(&mut byte).await? == 0 {
                if i == 0 {
                    return Ok(None);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated encrypted frame length",
                ));
            }
            let plain = transform(decrypt, &byte)?;
            let byte = plain[0];
            length |= usize::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                if length == 0 || length > MAX_FRAME_SIZE {
                    return Err(invalid("invalid encrypted frame size"));
                }
                let mut body = vec![0; length];
                self.stream.read_exact(&mut body).await?;
                return Ok(Some(transform(decrypt, &body)?));
            }
        }
        Err(invalid("encrypted frame length exceeds three bytes"))
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
