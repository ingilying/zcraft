//! Player movement, survival exhaustion, void/fall damage, death, and entity actions.
use super::drops::spawn_drop_scatter;
use crate::{
    game::{
        Hand, Player,
        components::{
            Dead, FallDistance, Health, Hunger, MovementFlags, NetworkEntityId, PlayerGameMode,
            PlayerInventory, PlayerName, PlayerViewState, Position, Rotation, add_exhaustion,
            apply_damage_at_tick, entity_pose,
        },
        io::GameOutput,
        resources::{EntityRegistry, OutputQueue, VoxelWorld},
    },
    world::{self, BlockPos},
};
use bevy_ecs::prelude::World;
use uuid::Uuid;

pub(crate) fn handle_move(
    ecs: &mut World,
    uuid: Uuid,
    position: Option<[f64; 3]>,
    rotation: Option<[f32; 2]>,
    on_ground: bool,
    tick: u64,
) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if ecs
        .get::<PlayerViewState>(entity)
        .is_none_or(|v| v.pending_teleport)
    {
        return;
    }

    let world = ecs.resource::<VoxelWorld>().0.clone();

    if let Some(pos) = position
        && !Player::position_in_bounds(pos)
    {
        let spawn = world.spawn_position();
        if let Some(mut p) = ecs.get_mut::<Position>(entity) {
            p.0 = spawn;
        }
        if let Some(mut r) = ecs.get_mut::<Rotation>(entity) {
            *r = Rotation::default();
        }
        if let Some(mut f) = ecs.get_mut::<MovementFlags>(entity) {
            f.on_ground = false;
        }
        if let Some(mut fall) = ecs.get_mut::<FallDistance>(entity) {
            fall.0 = 0.0;
        }
        if let Some(mut view) = ecs.get_mut::<PlayerViewState>(entity) {
            view.pending_teleport = true;
        }
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerTeleported {
                uuid,
                position: spawn,
                yaw: 0.0,
                pitch: 0.0,
            });
        return;
    }

    let Some(entity_id) = ecs.get::<NetworkEntityId>(entity).map(|id| id.0) else {
        return;
    };
    let Some(&mode) = ecs.get::<PlayerGameMode>(entity) else {
        return;
    };
    let Some(&old_pos) = ecs.get::<Position>(entity) else {
        return;
    };
    let Some(&old_flags) = ecs.get::<MovementFlags>(entity) else {
        return;
    };
    let is_dead = ecs.get::<Dead>(entity).is_some();

    let new_pos = position.unwrap_or(old_pos.0);
    if position.is_some()
        && let Some(mut p) = ecs.get_mut::<Position>(entity)
    {
        p.0 = new_pos;
    }
    let new_rot = {
        let Some(mut r) = ecs.get_mut::<Rotation>(entity) else {
            return;
        };
        if let Some([yaw, pitch]) = rotation {
            r.yaw = yaw;
            r.pitch = pitch;
        }
        *r
    };
    let new_flags = {
        let Some(mut f) = ecs.get_mut::<MovementFlags>(entity) else {
            return;
        };
        f.on_ground = on_ground;
        *f
    };

    let mut hurt_event = false;
    let mut vitals_update = None;
    let mut death_reason: Option<&'static str> = None;

    if mode.is_survival() && !is_dead {
        let [old_x, old_y, old_z] = old_pos.0;
        let [px, py, pz] = new_pos;
        let horiz = ((px - old_x).powi(2) + (pz - old_z).powi(2)).sqrt();
        if let Some(mut hunger) = ecs.get_mut::<Hunger>(entity) {
            if new_flags.sprinting && horiz > 0.0 && horiz < 10.0 {
                add_exhaustion(&mut hunger, mode, false, 0.1 * horiz as f32);
            }
            if old_flags.on_ground && !on_ground && py > old_y {
                add_exhaustion(
                    &mut hunger,
                    mode,
                    false,
                    if new_flags.sprinting { 0.2 } else { 0.05 },
                );
            }
        }

        let mut health = *ecs.get::<Health>(entity).expect("player health");
        let mut fall = *ecs.get::<FallDistance>(entity).expect("player fall distance");
        let (food, saturation) = ecs
            .get::<Hunger>(entity)
            .map_or((20, 5.0), |h| (h.food, h.saturation));

        if py < f64::from(world::MIN_Y) - 64.0 {
            let (damaged, died) =
                apply_damage_at_tick(&mut health, &mut fall, mode, false, 4.0, tick);
            if damaged {
                hurt_event = true;
                vitals_update = Some((health.current, food, saturation));
                if died {
                    death_reason = Some("fell out of the world");
                }
            }
        } else {
            let feet_pos = BlockPos([px.floor() as i32, py.floor() as i32, pz.floor() as i32]);
            let in_liquid = world
                .block_at_if_loaded(feet_pos)
                .and_then(crate::game::block_by_state)
                .is_some_and(|d| d.liquid);
            if in_liquid {
                fall.0 = 0.0;
            } else if on_ground {
                let fell = std::mem::take(&mut fall.0);
                if fell > 3.0 {
                    let damage = (fell - 3.0).ceil() as f32;
                    let (damaged, died) =
                        apply_damage_at_tick(&mut health, &mut fall, mode, false, damage, tick);
                    if damaged {
                        hurt_event = true;
                        vitals_update = Some((health.current, food, saturation));
                        if died {
                            death_reason = Some("fell from a high place");
                        }
                    }
                }
            } else if py < old_y {
                fall.0 += old_y - py;
            }
        }

        if let Some(mut h) = ecs.get_mut::<Health>(entity) {
            *h = health;
        }
        if let Some(mut f) = ecs.get_mut::<FallDistance>(entity) {
            *f = fall;
        }
    }

    let pose = entity_pose(Position(new_pos), new_rot, new_flags);
    {
        let mut out = ecs.resource_mut::<OutputQueue>();
        if hurt_event {
            out.push(GameOutput::PlayerHurt { uuid, entity_id });
        }
        if let Some((health, food, saturation)) = vitals_update {
            out.push(GameOutput::PlayerVitalsUpdated {
                uuid,
                health,
                food,
                saturation,
            });
        }
        out.push(GameOutput::PlayerPoseUpdated { uuid, pose });
    }

    if let Some(reason) = death_reason {
        if let Ok(mut e) = ecs.get_entity_mut(entity) {
            e.insert(Dead);
        }
        let player_name = ecs
            .get::<PlayerName>(entity)
            .map(|n| n.0.clone())
            .unwrap_or_default();
        let (dropped, inv_snapshot, main_hand, off_hand) = {
            let mut inv = ecs
                .get_mut::<PlayerInventory>(entity)
                .expect("player inventory");
            let dropped = inv.0.drain_all();
            let snapshot = inv.0.clone();
            let main = inv.0.hand(Hand::Main);
            let off = inv.0.hand(Hand::Off);
            (dropped, snapshot, main, off)
        };
        for stack in dropped {
            spawn_drop_scatter(
                ecs,
                [new_pos[0], new_pos[1] + 0.5, new_pos[2]],
                stack,
                40,
            );
        }
        let mut out = ecs.resource_mut::<OutputQueue>();
        out.push(GameOutput::PlayerInventorySynced {
            uuid,
            inventory: inv_snapshot,
        });
        out.push(GameOutput::PlayerEquipmentChanged {
            uuid,
            entity_id,
            main_hand,
            off_hand,
        });
        out.push(GameOutput::BroadcastSystemChat(format!(
            "{player_name} {reason}"
        )));
    }
}

pub(crate) fn handle_entity_action(ecs: &mut World, uuid: Uuid, action: i32) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    let Some(entity_id) = ecs.get::<NetworkEntityId>(entity).map(|id| id.0) else {
        return;
    };
    let changed = {
        let Some(mut flags) = ecs.get_mut::<MovementFlags>(entity) else {
            return;
        };
        let mut c = false;
        match action {
            0 if !flags.sneaking => {
                flags.sneaking = true;
                c = true;
            }
            1 if flags.sneaking => {
                flags.sneaking = false;
                c = true;
            }
            3 if !flags.sprinting => {
                flags.sprinting = true;
                c = true;
            }
            4 if flags.sprinting => {
                flags.sprinting = false;
                c = true;
            }
            _ => {}
        }
        c.then_some((flags.sneaking, flags.sprinting))
    };
    if let Some((sneaking, sprinting)) = changed {
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerMetadataChanged {
                uuid,
                entity_id,
                sneaking,
                sprinting,
            });
    }
}

pub(crate) fn handle_arm_animation(ecs: &mut World, uuid: Uuid, hand: i32) {
    let Some(&entity) = ecs.resource::<EntityRegistry>().players.get(&uuid) else {
        return;
    };
    if let Some(id) = ecs.get::<NetworkEntityId>(entity) {
        let entity_id = id.0;
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerArmSwing {
                uuid,
                entity_id,
                hand,
            });
    }
}
