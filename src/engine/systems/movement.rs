//! Player movement, survival exhaustion, void/fall damage, death, and entity actions.
use super::drops::spawn_drop_scatter;
use crate::{
    engine::{
        components::{PlayerData, PlayerInventory, PlayerViewState},
        io::GameOutput,
        resources::{EntityRegistry, OutputQueue, VoxelWorld},
    },
    game::{Hand, Player},
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
        if let Some(mut p) = ecs.get_mut::<PlayerData>(entity) {
            p.0.position = spawn;
            p.0.yaw = 0.0;
            p.0.pitch = 0.0;
            p.0.on_ground = false;
            p.0.fall_distance = 0.0;
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

    let mut hurt_event = false;
    let mut vitals_update = None;
    let mut death_info: Option<(String, [f64; 3], &'static str)> = None;
    let (entity_id, pose) = {
        let Some(mut p) = ecs.get_mut::<PlayerData>(entity) else {
            return;
        };
        let player = &mut p.0;
        let [old_x, old_y, old_z] = player.position;
        let was_on_ground = player.on_ground;
        if let Some([x, y, z]) = position {
            player.position = [x, y, z];
        }
        if let Some([yaw, pitch]) = rotation {
            player.yaw = yaw;
            player.pitch = pitch;
        }
        player.on_ground = on_ground;

        if player.is_survival() && !player.dead {
            let [px, py, pz] = player.position;
            let horiz = ((px - old_x).powi(2) + (pz - old_z).powi(2)).sqrt();
            if player.sprinting && horiz > 0.0 && horiz < 10.0 {
                player.add_exhaustion(0.1 * horiz as f32);
            }
            if was_on_ground && !on_ground && py > old_y {
                player.add_exhaustion(if player.sprinting { 0.2 } else { 0.05 });
            }
            if py < f64::from(world::MIN_Y) - 64.0 {
                if player.apply_damage_at_tick(4.0, tick) {
                    hurt_event = true;
                    vitals_update = Some((player.health, player.food, player.saturation));
                    if player.dead {
                        death_info =
                            Some((player.name.clone(), player.position, "fell out of the world"));
                    }
                }
            } else {
                let feet_pos = BlockPos([px.floor() as i32, py.floor() as i32, pz.floor() as i32]);
                let in_liquid = world
                    .block_at_if_loaded(feet_pos)
                    .and_then(crate::game::block_by_state)
                    .is_some_and(|d| d.liquid);
                if in_liquid {
                    player.fall_distance = 0.0;
                } else if on_ground {
                    let fall = std::mem::take(&mut player.fall_distance);
                    if fall > 3.0 {
                        let damage = (fall - 3.0).ceil() as f32;
                        if player.apply_damage_at_tick(damage, tick) {
                            hurt_event = true;
                            vitals_update = Some((player.health, player.food, player.saturation));
                            if player.dead {
                                death_info = Some((
                                    player.name.clone(),
                                    player.position,
                                    "fell from a high place",
                                ));
                            }
                        }
                    }
                } else if py < old_y {
                    player.fall_distance += old_y - py;
                }
            }
        }
        (player.entity_id, player.pose())
    };

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

    if let Some((player_name, death_pos, reason)) = death_info {
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
                [death_pos[0], death_pos[1] + 0.5, death_pos[2]],
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
    let changed = {
        let Some(mut p) = ecs.get_mut::<PlayerData>(entity) else {
            return;
        };
        let mut c = false;
        match action {
            0 if !p.0.sneaking => {
                p.0.sneaking = true;
                c = true;
            }
            1 if p.0.sneaking => {
                p.0.sneaking = false;
                c = true;
            }
            3 if !p.0.sprinting => {
                p.0.sprinting = true;
                c = true;
            }
            4 if p.0.sprinting => {
                p.0.sprinting = false;
                c = true;
            }
            _ => {}
        }
        c.then_some((p.0.entity_id, p.0.sneaking, p.0.sprinting))
    };
    if let Some((entity_id, sneaking, sprinting)) = changed {
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
    if let Some(p) = ecs.get::<PlayerData>(entity) {
        let entity_id = p.0.entity_id;
        ecs.resource_mut::<OutputQueue>()
            .push(GameOutput::PlayerArmSwing {
                uuid,
                entity_id,
                hand,
            });
    }
}
