//! Decoupled `bevy_ecs` game engine isolating all authoritative gameplay logic
//! from network IO, sockets, and protocol framing.
pub mod components;
mod engine;
pub mod io;
pub mod resources;
mod systems;
#[cfg(test)]
mod tests;

pub use engine::GameEngine;
pub use io::{
    ContainerKind, ContainerOpenSync, DropSnapshot, GameInput, GameOutput, OpenContainer,
    PlayerSnapshot,
};
