//! Authoritative server-side player gameplay state, independent of transport.
use crate::{
    config::GameMode,
    protocol::packets::play_entities::EntityPose,
    world::{self, BlockPos},
};
use uuid::Uuid;

pub const PLAYER_EYE_HEIGHT: f64 = 1.62;
pub const PLAYER_HALF_WIDTH: f64 = 0.3;
pub const PLAYER_HEIGHT: f64 = 1.8;
pub const BLOCK_REACH: f64 = 5.0;

#[derive(Debug, Clone)]
pub struct Player {
    pub entity_id: i32,
    pub uuid: Uuid,
    pub name: String,
    pub game_mode: GameMode,
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub sneaking: bool,
    pub sprinting: bool,
}

impl Player {
    pub fn at_spawn(entity_id: i32, uuid: Uuid, name: String, game_mode: GameMode) -> Self {
        Self {
            entity_id,
            uuid,
            name,
            game_mode,
            position: [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z],
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            sneaking: false,
            sprinting: false,
        }
    }

    pub fn is_creative(&self) -> bool {
        self.game_mode == GameMode::Creative
    }

    pub fn eye_position(&self) -> [f64; 3] {
        [
            self.position[0],
            self.position[1] + PLAYER_EYE_HEIGHT,
            self.position[2],
        ]
    }

    pub fn in_reach(&self, pos: BlockPos) -> bool {
        self.eye_position()
            .into_iter()
            .zip(pos.0)
            .map(|(p, b)| {
                let b = f64::from(b);
                (p - p.clamp(b, b + 1.0)).powi(2)
            })
            .sum::<f64>()
            <= BLOCK_REACH * BLOCK_REACH
    }

    pub fn intersects_block(&self, pos: BlockPos) -> bool {
        let [x, y, z] = self.position;
        let [bx, by, bz] = pos.0.map(f64::from);
        x + PLAYER_HALF_WIDTH > bx
            && x - PLAYER_HALF_WIDTH < bx + 1.0
            && y + PLAYER_HEIGHT > by
            && y < by + 1.0
            && z + PLAYER_HALF_WIDTH > bz
            && z - PLAYER_HALF_WIDTH < bz + 1.0
    }

    pub fn position_in_bounds([x, y, z]: [f64; 3]) -> bool {
        (-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&x)
            && (-world::WORLD_LIMIT..=world::WORLD_LIMIT).contains(&z)
            && (f64::from(world::MIN_Y)..=f64::from(world::MIN_Y + world::HEIGHT)).contains(&y)
    }

    pub fn reset_to_spawn(&mut self) {
        self.position = [world::SPAWN_X, world::SPAWN_Y, world::SPAWN_Z];
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.on_ground = false;
    }

    pub fn pose(&self) -> EntityPose {
        EntityPose::new(self.position, self.yaw, self.pitch, self.on_ground)
    }
}
