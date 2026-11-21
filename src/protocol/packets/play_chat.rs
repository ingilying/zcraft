//! Minecraft Java 1.21.1 (767) chat framing and a static Brigadier command tree.
use crate::protocol::{Decode, Encode, Reader, invalid, put_string, put_varint};
use std::io;

const MAX_INPUT: usize = 256;
const MAX_SIGNATURES: i32 = 8;
const SIGNATURE_BYTES: usize = 256;
const LAST_SEEN_BYTES: usize = 3;

#[derive(Debug, PartialEq, Eq)]
pub struct ChatAcknowledgement;
impl Decode for ChatAcknowledgement {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        read_offset(reader)?;
        Ok(Self)
    }
}

fn read_offset(reader: &mut Reader<'_>) -> io::Result<()> {
    if reader.varint()? < 0 {
        return Err(invalid("negative chat acknowledgement offset"));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct ChatMessage(pub String);
impl Decode for ChatMessage {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let message = reader.string(MAX_INPUT)?;
        reader.take(16)?; // Timestamp and salt.
        match reader.take(1)?[0] {
            0 => {}
            1 => {
                reader.take(SIGNATURE_BYTES)?;
            }
            _ => return Err(invalid("invalid chat signature flag")),
        }
        read_offset(reader)?;
        reader.take(LAST_SEEN_BYTES)?;
        Ok(Self(message))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ChatCommand(pub String);
impl Decode for ChatCommand {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self(reader.string(MAX_INPUT)?))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SignedChatCommand(pub String);
impl Decode for SignedChatCommand {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        let command = reader.string(MAX_INPUT)?;
        reader.take(16)?; // Timestamp and salt.
        let count = reader.varint()?;
        if !(0..=MAX_SIGNATURES).contains(&count) {
            return Err(invalid("invalid command signature count"));
        }
        for _ in 0..count {
            reader.string(16)?;
            reader.take(SIGNATURE_BYTES)?;
        }
        read_offset(reader)?;
        reader.take(LAST_SEEN_BYTES)?;
        Ok(Self(command))
    }
}

/// Plain server text, not a signed or player-chat packet.
#[derive(Debug, PartialEq, Eq)]
pub struct SystemChat(pub String);
impl Encode for SystemChat {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        super::common::put_nbt_text(&self.0, out)?;
        out.push(0); // Do not overlay the action bar.
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct DeclareCommands(Vec<Node>);

#[derive(Debug, PartialEq, Eq)]
struct Node {
    name: Option<String>,
    argument: Option<crate::command::ArgumentKind>,
    children: Vec<i32>,
}

impl DeclareCommands {
    pub fn new(
        registry: &crate::command::CommandRegistry,
        source: &crate::command::CommandSource,
    ) -> Self {
        let mut nodes = vec![Node {
            name: None,
            argument: None,
            children: Vec::new(),
        }];
        for command in registry.available(source) {
            let literal_start = nodes.len();
            let literal_count = 1 + command.aliases.len();
            let argument_start = literal_start + literal_count;
            for name in std::iter::once(&command.name).chain(&command.aliases) {
                let index = nodes.len() as i32;
                nodes[0].children.push(index);
                nodes.push(Node {
                    name: Some(name.clone()),
                    argument: None,
                    children: if command.arguments.is_empty() {
                        vec![]
                    } else {
                        vec![argument_start as i32]
                    },
                });
            }
            for (index, argument) in command.arguments.iter().enumerate() {
                nodes.push(Node {
                    name: Some(argument.name.clone()),
                    argument: Some(argument.kind.clone()),
                    children: if index + 1 == command.arguments.len() {
                        vec![]
                    } else {
                        vec![(argument_start + index + 1) as i32]
                    },
                });
            }
        }
        Self(nodes)
    }
}

impl Encode for DeclareCommands {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        use crate::command::ArgumentKind;
        put_varint(self.0.len() as i32, out);
        for node in &self.0 {
            let kind = if node.name.is_none() {
                0
            } else if node.argument.is_some() {
                2
            } else {
                1
            };
            out.push(
                kind | if kind != 0 && node.children.is_empty() {
                    4
                } else {
                    0
                },
            );
            put_varint(node.children.len() as i32, out);
            for &child in &node.children {
                put_varint(child, out);
            }
            if let Some(name) = &node.name {
                put_string(name, out)?;
            }
            if let Some(argument) = &node.argument {
                match argument {
                    ArgumentKind::Bool => put_varint(0, out), // brigadier:bool
                    ArgumentKind::Integer { min, max } => {
                        put_varint(3, out); // brigadier:integer
                        out.push(3); // Both bounds.
                        out.extend_from_slice(&min.to_be_bytes());
                        out.extend_from_slice(&max.to_be_bytes());
                    }
                    ArgumentKind::Word | ArgumentKind::String | ArgumentKind::Greedy => {
                        put_varint(5, out); // brigadier:string
                        put_varint(
                            match argument {
                                ArgumentKind::Word => 0,
                                ArgumentKind::String => 1,
                                _ => 2,
                            },
                            out,
                        );
                    }
                }
            }
        }
        put_varint(0, out);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Clientbound, State, decode};

    #[test]
    fn chat_message_is_bounded_and_optional_signature_is_consumed() {
        let mut packet = vec![6];
        let message = "🙂".repeat(128);
        put_string(&message, &mut packet).unwrap();
        packet.extend([0; 16]);
        packet.push(1);
        packet.extend([0; SIGNATURE_BYTES + 1 + LAST_SEEN_BYTES]);
        assert_eq!(
            decode(State::Play, &packet).unwrap(),
            ChatMessage(message).into()
        );
        for size in 0..packet.len() {
            assert!(decode(State::Play, &packet[..size]).is_err());
        }
        packet.push(0);
        assert!(decode(State::Play, &packet).is_err());
        let mut oversized = vec![4];
        put_string(&"🙂".repeat(129), &mut oversized).unwrap();
        assert!(decode(State::Play, &oversized).is_err());
        assert!(decode(State::Play, &[3, 0]).is_ok());
        assert!(decode(State::Play, &[3, 255, 255, 255, 255, 15]).is_err());
    }

    #[test]
    fn signed_command_consumes_bounded_arguments_without_using_signatures() {
        let mut packet = vec![5];
        put_string("me waves", &mut packet).unwrap();
        packet.extend([0; 16]);
        let count = packet.len();
        packet.push(1);
        put_string("action", &mut packet).unwrap();
        packet.extend([1; SIGNATURE_BYTES]);
        packet.extend([0; 1 + LAST_SEEN_BYTES]);
        assert_eq!(
            decode(State::Play, &packet).unwrap(),
            SignedChatCommand("me waves".into()).into(),
        );
        assert!(decode(State::Play, &packet[..packet.len() - 1]).is_err());
        packet[count] = 9;
        assert!(decode(State::Play, &packet).is_err());
    }

    #[test]
    fn system_text_has_anonymous_modified_utf8_nbt_and_no_overlay() {
        let encoded = Clientbound::from(SystemChat("你🙂".into()))
            .encode()
            .unwrap();
        assert_eq!(
            encoded,
            [
                0x6c, 8, 0, 9, 0xe4, 0xbd, 0xa0, 0xed, 0xa0, 0xbd, 0xed, 0xb9, 0x82, 0
            ]
        );
    }
}
