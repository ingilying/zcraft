//! Declarative `bevy_ecs` system for block digging/placing, item use,
//! hotbar selection, creative slots, containers (Chest, Crafting Table, Furnace),
//! and player inventory clicks.
mod blocks;
mod containers;
mod items;

use crate::game::{
    Hand, ItemStack, Player, UseContext,
    components::{
        Dead, EatingState, EntityUuid, Health, Hunger, MiningState, MovementState, NetworkEntityId,
        PlayerContainer, PlayerInventory, PlayerMarker, PlayerProfile, PlayerViewState, Position,
        Rotation, player_eye_position,
    },
    events::{InteractionMsg, SpawnDropMsg},
    io::GameOutput,
    resources::{ChestViewers, EntityRegistry, OutputQueue, TickClock, VoxelWorld},
};
use bevy_ecs::{
    prelude::{Commands, Entity, MessageReader, MessageWriter, Query, Res, ResMut, With},
    query::QueryData,
    system::SystemParam,
};
use uuid::Uuid;

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct InteractingPlayer {
    pub(super) net_id: &'static NetworkEntityId,
    pub(super) uuid: &'static EntityUuid,
    pub(super) profile: &'static mut PlayerProfile,
    pub(super) pos: &'static mut Position,
    pub(super) rot: &'static mut Rotation,
    pub(super) movement: &'static mut MovementState,
    pub(super) health: &'static mut Health,
    pub(super) hunger: &'static mut Hunger,
    pub(super) dead: Option<&'static Dead>,
    pub(super) eating: Option<&'static EatingState>,
    pub(super) mining: Option<&'static MiningState>,
    pub(super) inv: &'static mut PlayerInventory,
    pub(super) container: &'static mut PlayerContainer,
    pub(super) view: &'static PlayerViewState,
}

/// Bundled declarative ECS parameters shared across all player interaction handlers.
#[derive(SystemParam)]
pub(crate) struct InteractionCtx<'w, 's> {
    pub(super) clock: Res<'w, TickClock>,
    pub(super) world: Res<'w, VoxelWorld>,
    pub(super) registry: Res<'w, EntityRegistry>,
    pub(super) viewers: ResMut<'w, ChestViewers>,
    pub(super) players: Query<'w, 's, InteractingPlayer, With<PlayerMarker>>,
    pub(super) commands: Commands<'w, 's>,
    pub(super) drops: MessageWriter<'w, SpawnDropMsg>,
    pub(super) out: ResMut<'w, OutputQueue>,
}

impl InteractionCtx<'_, '_> {
    pub(super) fn player_entity(&self, uuid: Uuid) -> Option<Entity> {
        self.registry.players.get(&uuid).copied()
    }

    pub(super) fn emit_equipment(&mut self, uuid: Uuid, entity: Entity) {
        let Ok(p) = self.players.get(entity) else {
            return;
        };
        self.out.push(GameOutput::PlayerEquipmentChanged {
            uuid,
            entity_id: p.net_id.0,
            main_hand: p.inv.0.hand(Hand::Main),
            off_hand: p.inv.0.hand(Hand::Off),
        });
    }

    pub(super) fn emit_inventory_if_can_interact(&mut self, uuid: Uuid, entity: Entity) {
        let Ok(p) = self.players.get(entity) else {
            return;
        };
        if !p.profile.can_interact() {
            return;
        }
        self.out.push(GameOutput::PlayerInventorySynced {
            uuid,
            inventory: p.inv.0.clone(),
        });
    }

    pub(super) fn throw_from_player(&mut self, entity: Entity, stack: ItemStack) {
        let Ok(p) = self.players.get(entity) else {
            return;
        };
        let eye = player_eye_position(*p.pos);
        let yaw_rad = f64::from(p.rot.yaw).to_radians();
        let pitch_rad = f64::from(p.rot.pitch).to_radians();
        let vx = -yaw_rad.sin() * pitch_rad.cos() * 0.3;
        let vy = -pitch_rad.sin() * 0.3 + 0.1;
        let vz = yaw_rad.cos() * pitch_rad.cos() * 0.3;
        self.drops
            .write(SpawnDropMsg::with_velocity(eye, [vx, vy, vz], stack, 40));
    }

    pub(super) fn allocate_window_id(&mut self, entity: Entity) -> u8 {
        let mut p = self.players.get_mut(entity).expect("player entity");
        let id = p.container.next_window_id;
        p.container.next_window_id = if id >= 100 { 1 } else { id + 1 };
        id
    }

    /// Execute a `BlockBehavior` / `ItemBehavior` callback with zero-copy access to
    /// the player's `Inventory` and automatic write-back of mutated `Player` fields.
    pub(super) fn with_use_context<R>(
        &mut self,
        entity: Entity,
        f: impl FnOnce(&mut UseContext<'_>) -> R,
    ) -> Option<R> {
        let world = self.world.0.clone();
        let tick = self.clock.tick;
        let Ok(mut p) = self.players.get_mut(entity) else {
            return None;
        };

        let mut player = Player {
            entity_id: p.net_id.0,
            uuid: p.uuid.0,
            name: p.profile.name.clone(),
            game_mode: p.profile.game_mode,
            position: p.pos.0,
            yaw: p.rot.yaw,
            pitch: p.rot.pitch,
            on_ground: p.movement.on_ground,
            sneaking: p.movement.sneaking,
            sprinting: p.movement.sprinting,
            health: p.health.current,
            food: p.hunger.food,
            saturation: p.hunger.saturation,
            exhaustion: p.hunger.exhaustion,
            food_tick_timer: p.hunger.tick_timer,
            eating: p
                .eating
                .map(|e| (e.hand, e.item, e.start_tick, e.required_ticks)),
            fall_distance: p.movement.fall_distance,
            dead: p.dead.is_some(),
            last_hurt_tick: p.health.last_hurt_tick,
            respawn_point: p.profile.respawn_point,
        };

        let result = {
            let mut use_ctx = UseContext {
                world: &world,
                player: &mut player,
                inventory: &mut p.inv.0,
                tick,
            };
            f(&mut use_ctx)
        };

        p.pos.0 = player.position;
        p.rot.yaw = player.yaw;
        p.rot.pitch = player.pitch;
        p.movement.on_ground = player.on_ground;
        p.movement.sneaking = player.sneaking;
        p.movement.sprinting = player.sprinting;
        p.movement.fall_distance = player.fall_distance;
        p.profile.game_mode = player.game_mode;
        p.profile.respawn_point = player.respawn_point;
        p.health.current = player.health;
        p.health.last_hurt_tick = player.last_hurt_tick;
        p.hunger.food = player.food;
        p.hunger.saturation = player.saturation;
        p.hunger.exhaustion = player.exhaustion;
        p.hunger.tick_timer = player.food_tick_timer;

        if player.dead {
            self.commands.entity(entity).insert(Dead);
        } else if p.dead.is_some() {
            self.commands.entity(entity).remove::<Dead>();
        }
        if let Some((hand, item, start_tick, required_ticks)) = player.eating {
            self.commands.entity(entity).insert(EatingState {
                hand,
                item,
                start_tick,
                required_ticks,
            });
        } else if p.eating.is_some() {
            self.commands.entity(entity).remove::<EatingState>();
        }

        Some(result)
    }
}

/// Pure declarative ECS system consuming `InteractionMsg` events for digging,
/// placing, item use, hotbar selection, creative slots, and container clicks.
pub(crate) fn player_interaction_system(
    mut messages: MessageReader<InteractionMsg>,
    mut ctx: InteractionCtx,
) {
    let tick = ctx.clock.tick;
    for msg in messages.read() {
        match *msg {
            InteractionMsg::Dig {
                uuid,
                action,
                pos,
                face,
                sequence,
            } => blocks::handle_dig(&mut ctx, uuid, action, pos, face, sequence, tick),
            InteractionMsg::Place {
                uuid,
                pos,
                face,
                hand,
                hit,
                sequence,
            } => blocks::handle_place(&mut ctx, uuid, pos, face, hand, hit, sequence),
            InteractionMsg::UseItem {
                uuid,
                hand,
                sequence,
            } => items::handle_use_item(&mut ctx, uuid, hand, sequence),
            InteractionMsg::SelectSlot { uuid, slot } => {
                items::handle_select_slot(&mut ctx, uuid, slot)
            }
            InteractionMsg::CreativeSlot { uuid, slot, item } => {
                items::handle_creative_slot(&mut ctx, uuid, slot, item)
            }
            InteractionMsg::InventoryClick {
                uuid,
                window,
                state_id,
                click,
            } => containers::handle_inventory_click(&mut ctx, uuid, window, state_id, click),
            InteractionMsg::CloseWindow { uuid, window_id } => {
                containers::handle_close_window(&mut ctx, uuid, window_id)
            }
        }
    }
}
