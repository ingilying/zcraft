//! Uncompressed Minecraft Java protocol; packet IDs live in the versioned registry.
pub mod codec;
pub mod frame;
pub mod packets;
pub mod registry;

pub use codec::{Decode, Encode, Reader, invalid, put_string, put_varint};
pub use frame::{MAX_FRAME_SIZE, read_frame, write_frame};
pub use registry::{Clientbound, Serverbound, decode};

pub const PROTOCOL_VERSION: i32 = 767;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Handshake,
    Status,
    Login,
    Configuration,
    Play,
}
