use tokio::io::{AsyncRead, AsyncWrite};
use zcraft::{config::DEFAULT_COMPRESSION_THRESHOLD, protocol::Reader, transport::Transport};

pub async fn negotiate_compression<S: AsyncRead + AsyncWrite + Unpin>(client: &mut Transport<S>) {
    let offer = client.read_frame().await.unwrap().unwrap();
    let mut reader = Reader::new(&offer);
    assert_eq!(reader.varint().unwrap(), 3); // Login Set Compression, still old framing.
    let threshold = reader.varint().unwrap();
    assert_eq!(threshold, DEFAULT_COMPRESSION_THRESHOLD);
    reader.finish().unwrap();
    client.enable_compression(threshold as usize).unwrap();
}
