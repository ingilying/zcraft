//! Minecraft Java 1.21.1 (767) chat and command packets.
use crate::protocol::{Decode, Encode, Reader, invalid, put_string, put_varint};
use std::io;

const MAX_INPUT: usize = 256;
pub const MAX_COMPLETION_INPUT: usize = 32500;

#[derive(Debug, PartialEq, Eq)]
pub struct CommandCompletionRequest {
    pub transaction: i32,
    pub input: String,
}
impl Decode for CommandCompletionRequest {
    fn decode(reader: &mut Reader<'_>) -> io::Result<Self> {
        Ok(Self {
            transaction: reader.varint()?,
            input: reader.string(MAX_COMPLETION_INPUT)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct CommandCompletionResponse {
    pub transaction: i32,
    pub matches: Vec<crate::command::Suggestion>,
}
impl Encode for CommandCompletionResponse {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        use crate::command::{MAX_SUGGESTION_BYTES, MAX_SUGGESTIONS};
        let span = self
            .matches
            .first()
            .map_or(0..0, |first| first.span.clone());
        if self.matches.len() > MAX_SUGGESTIONS
            || span.start > span.end
            || span.end > MAX_COMPLETION_INPUT
            || self.matches.iter().any(|s| {
                s.span != span
                    || s.text.len() > MAX_SUGGESTION_BYTES
                    || s.tooltip
                        .as_ref()
                        .is_some_and(|text| text.len() > MAX_SUGGESTION_BYTES)
            })
        {
            return Err(invalid("invalid command suggestions"));
        }
        put_varint(self.transaction, out);
        put_varint(span.start as i32, out);
        put_varint(span.len() as i32, out);
        put_varint(self.matches.len() as i32, out);
        for suggestion in &self.matches {
            put_string(&suggestion.text, out)?;
            out.push(u8::from(suggestion.tooltip.is_some()));
            if let Some(tooltip) = &suggestion.tooltip {
                super::common::put_nbt_text(tooltip, out)?;
            }
        }
        Ok(())
    }
}
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
pub struct DeclareCommands(Vec<WireNode>);

#[derive(Debug, PartialEq, Eq)]
struct WireNode {
    flags: u8,
    name: String,
    parser: Option<(i32, Vec<u8>)>,
    redirect: Option<i32>,
    children: Vec<i32>,
}

impl DeclareCommands {
    pub fn new(
        registry: &crate::command::CommandRegistry,
        source: &crate::command::CommandSource,
    ) -> Self {
        use crate::command::{NodeId, NodeKind};
        // Only expose reachable, usable nodes with a client-side parser.
        // Every redirect needs a target that is independently visible.
        let mut ids = vec![NodeId::ROOT];
        let mut index = std::collections::HashMap::from([(NodeId::ROOT, 0i32)]);
        let mut i = 0;
        while let Some(&id) = ids.get(i) {
            for &child in registry.node(id).expect("registered node").children() {
                let node = registry.node(child).expect("registered child");
                if node.can_use(source)
                    && !matches!(node.kind(), NodeKind::Argument(kind) if parser(kind).is_none())
                    && !index.contains_key(&child)
                {
                    index.insert(child, ids.len() as i32);
                    ids.push(child);
                }
            }
            i += 1;
        }
        // Remove redirects to nodes absent from the filtered tree without
        // discarding an independently executable literal.
        Self(
            ids.into_iter()
                .map(|id| {
                    let node = registry.node(id).expect("registered node");
                    let redirect = node.redirect().and_then(|r| index.get(&r.target).copied());
                    let (kind, parser) = match node.kind() {
                        NodeKind::Root => (0, None),
                        NodeKind::Literal => (1, None),
                        NodeKind::Argument(kind) => (2, parser(kind)),
                    };
                    WireNode {
                        flags: kind
                            | if node.executable() { 4 } else { 0 }
                            | if redirect.is_some() { 8 } else { 0 }
                            | if kind == 2 && node.has_suggestion_provider() {
                                16
                            } else {
                                0
                            },
                        name: node.name().into(),
                        parser,
                        redirect,
                        children: node
                            .children()
                            .iter()
                            .filter_map(|id| index.get(id).copied())
                            .collect(),
                    }
                })
                .collect(),
        )
    }
}

fn parser(kind: &crate::command::ArgumentKind) -> Option<(i32, Vec<u8>)> {
    use crate::command::ArgumentKind::*;
    fn bounds<const N: usize>(lower: Option<[u8; N]>, upper: Option<[u8; N]>) -> Vec<u8> {
        let mut bytes = vec![u8::from(lower.is_some()) | (u8::from(upper.is_some()) << 1)];
        bytes.extend(lower.into_iter().flatten());
        bytes.extend(upper.into_iter().flatten());
        bytes
    }
    Some(match kind {
        Bool => (0, vec![]),
        Float { min, max } => (
            1,
            bounds(
                (*min != -f32::MAX).then(|| min.to_be_bytes()),
                (*max != f32::MAX).then(|| max.to_be_bytes()),
            ),
        ),
        Double { min, max } => (
            2,
            bounds(
                (*min != -f64::MAX).then(|| min.to_be_bytes()),
                (*max != f64::MAX).then(|| max.to_be_bytes()),
            ),
        ),
        Integer { min, max } => (
            3,
            bounds(
                (*min != i32::MIN).then(|| min.to_be_bytes()),
                (*max != i32::MAX).then(|| max.to_be_bytes()),
            ),
        ),
        Long { min, max } => (
            4,
            bounds(
                (*min != i64::MIN).then(|| min.to_be_bytes()),
                (*max != i64::MAX).then(|| max.to_be_bytes()),
            ),
        ),
        Word => (5, vec![0]),
        String => (5, vec![1]),
        Greedy => (5, vec![2]),
        Custom(parser) => {
            return parser
                .wire()
                .filter(|(id, body)| (0..=53).contains(id) && body.len() <= 1024);
        }
    })
}

impl Encode for DeclareCommands {
    fn encode(&self, out: &mut Vec<u8>) -> io::Result<()> {
        put_varint(self.0.len() as i32, out);
        for node in &self.0 {
            out.push(node.flags);
            put_varint(node.children.len() as i32, out);
            for &child in &node.children {
                put_varint(child, out);
            }
            if let Some(redirect) = node.redirect {
                put_varint(redirect, out);
            }
            if node.flags & 3 != 0 {
                put_string(&node.name, out)?;
            }
            if let Some((parser, properties)) = &node.parser {
                put_varint(*parser, out);
                out.extend(properties);
                if node.flags & 16 != 0 {
                    put_string("minecraft:ask_server", out)?;
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
    fn completion_packets_validate_structure_and_encode_utf16_ranges() {
        let mut body = vec![0x0b, 17];
        put_string("/test 🙂", &mut body).unwrap();
        assert_eq!(
            decode(State::Play, &body).unwrap(),
            CommandCompletionRequest {
                transaction: 17,
                input: "/test 🙂".into()
            }
            .into()
        );
        for n in 0..body.len() {
            assert!(decode(State::Play, &body[..n]).is_err());
        }
        body.push(0);
        assert!(decode(State::Play, &body).is_err());
        let mut body = vec![0x0b, 0];
        put_string(&"x".repeat(MAX_COMPLETION_INPUT + 1), &mut body).unwrap();
        assert!(decode(State::Play, &body).is_err());

        let response = CommandCompletionResponse {
            transaction: 17,
            matches: vec![crate::command::Suggestion {
                span: 6..8,
                text: "x".into(),
                tooltip: Some("提示".into()),
            }],
        };
        let encoded = Clientbound::from(response).encode().unwrap();
        assert_eq!(&encoded[..10], &[0x10, 17, 6, 2, 1, 1, b'x', 1, 8, 0]);
        let invalid = CommandCompletionResponse {
            transaction: 17,
            matches: vec![crate::command::Suggestion {
                span: usize::MAX..usize::MAX,
                text: "x".into(),
                tooltip: None,
            }],
        };
        assert!(Clientbound::from(invalid).encode().is_err());
    }

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
