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
/// Vanilla 1.21.1 `Attributes.BLOCK_INTERACTION_RANGE` (4.5 Survival, 5.0 Creative)
/// plus `ServerPlayerGameMode`'s +1.0 server reach tolerance.
pub const SURVIVAL_BLOCK_REACH: f64 = 5.5;
pub const CREATIVE_BLOCK_REACH: f64 = 6.0;
/// Vanilla `LivingEntity.invulnerableDuration / 2` = 10 ticks (500 ms at 20 TPS).
pub const HURT_INVULNERABILITY: std::time::Duration = std::time::Duration::from_millis(500);

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
    pub health: f32,
    pub food: i32,
    pub saturation: f32,
    pub fall_distance: f64,
    pub dead: bool,
    pub last_hurt_at: Option<tokio::time::Instant>,
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
            health: 20.0,
            food: 20,
            saturation: 5.0,
            fall_distance: 0.0,
            dead: false,
            last_hurt_at: None,
        }
    }

    pub fn is_creative(&self) -> bool {
        self.game_mode == GameMode::Creative
    }

    pub fn is_survival(&self) -> bool {
        self.game_mode == GameMode::Survival
    }

    pub fn can_interact(&self) -> bool {
        matches!(self.game_mode, GameMode::Creative | GameMode::Survival)
    }

    pub fn block_reach(&self) -> f64 {
        if self.is_creative() {
            CREATIVE_BLOCK_REACH
        } else {
            SURVIVAL_BLOCK_REACH
        }
    }

    /// Apply damage in Survival mode respecting the 10-tick (500ms) hurt invulnerability
    /// window. Returns `true` if health decreased.
    pub fn apply_damage_at(&mut self, amount: f32, now: tokio::time::Instant) -> bool {
        if let Some(last) = self.last_hurt_at
            && now.duration_since(last) < HURT_INVULNERABILITY
        {
            return false;
        }
        if self.apply_damage(amount) {
            self.last_hurt_at = Some(now);
            true
        } else {
            false
        }
    }

    /// Apply damage in Survival mode without checking invulnerability timer.
    pub fn apply_damage(&mut self, amount: f32) -> bool {
        if !self.is_survival() || self.dead || amount <= 0.0 || !amount.is_finite() {
            return false;
        }
        self.health = (self.health - amount).max(0.0);
        if self.health <= 0.0 {
            self.dead = true;
            self.fall_distance = 0.0;
        }
        true
    }

    pub fn respawn_at(&mut self, pos: [f64; 3]) {
        self.position = pos;
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.on_ground = false;
        self.sneaking = false;
        self.sprinting = false;
        self.health = 20.0;
        self.food = 20;
        self.saturation = 5.0;
        self.fall_distance = 0.0;
        self.dead = false;
        self.last_hurt_at = None;
    }

    pub fn eye_position(&self) -> [f64; 3] {
        [
            self.position[0],
            self.position[1] + PLAYER_EYE_HEIGHT,
            self.position[2],
        ]
    }

    pub fn in_reach(&self, pos: BlockPos) -> bool {
        let reach = self.block_reach();
        self.eye_position()
            .into_iter()
            .zip(pos.0)
            .map(|(p, b)| {
                let b = f64::from(b);
                (p - p.clamp(b, b + 1.0)).powi(2)
            })
            .sum::<f64>()
            <= reach * reach
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
            && y.is_finite()
            && y.abs() <= 20_000_000.0
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
