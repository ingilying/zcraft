//! Authoritative `bevy_ecs` game simulation and data-driven Minecraft 1.21.1
//! gameplay rules, isolated from network IO, sockets, and protocol framing.
pub mod components;
mod engine;
pub mod events;
pub mod io;
pub mod resources;
pub mod rules;
mod systems;
#[cfg(test)]
mod tests;

pub use engine::GameEngine;
pub use io::{
    ContainerKind, ContainerOpenSync, DropSnapshot, GameInput, GameOutput, OpenContainer,
    PlayerSnapshot,
};
pub use rules::*;
