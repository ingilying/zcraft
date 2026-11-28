//! Declarative ECS system for player movement, survival exhaustion, void/fall
//! damage, death, entity actions, arm swings, and respawn.
use crate::{
    game::{
        Hand, Player,
        components::{
            Dead, EatingState, Health, Hunger, MiningState, MovementState, NetworkEntityId,
            PlayerContainer, PlayerInventory, PlayerMarker, PlayerProfile, PlayerViewState,
            Position, Rotation, add_exhaustion, apply_damage_at_tick, entity_pose,
        },
        events::{MovementMsg, SpawnDropMsg},
        io::GameOutput,
        resources::{EntityRegistry, OutputQueue, TickClock, VoxelWorld},
    },
    world::{self, BlockPos},
};
use bevy_ecs::{
    prelude::{Commands, MessageReader, MessageWriter, Query, Res, ResMut, With},
    query::QueryData,
};

#[derive(QueryData)]
#[query_data(mutable)]
pub(crate) struct MovingPlayer {
    net_id: &'static NetworkEntityId,
    profile: &'static mut PlayerProfile,
    pos: &'static mut Position,
    rot: &'static mut Rotation,
    movement: &'static mut MovementState,
    health: &'static mut Health,
    hunger: &'static mut Hunger,
    dead: Option<&'static Dead>,
    inv: &'static mut PlayerInventory,
    container: &'static mut PlayerContainer,
    view: &'static mut PlayerViewState,
}

/// Process player movement, posture flags, arm animation, and respawn requests.
#[allow(clippy::too_many_arguments)]
pub(crate) fn player_movement_system(
    mut messages: MessageReader<MovementMsg>,
    clock: Res<TickClock>,
    world: Res<VoxelWorld>,
    registry: Res<EntityRegistry>,
    mut players: Query<MovingPlayer, With<PlayerMarker>>,
    mut commands: Commands,
    mut drops: MessageWriter<SpawnDropMsg>,
    mut out: ResMut<OutputQueue>,
) {
    let tick = clock.tick;
    for msg in messages.read() {
        match *msg {
            MovementMsg::Move {
                uuid,
                position,
                rotation,
                on_ground,
            } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok(mut p) = players.get_mut(entity) else {
                    continue;
                };
                if p.view.pending_teleport {
                    continue;
                }

                if let Some(target_pos) = position
                    && !Player::position_in_bounds(target_pos)
                {
                    let spawn = world.0.spawn_position();
                    p.pos.0 = spawn;
                    *p.rot = Rotation::default();
                    p.movement.on_ground = false;
                    p.movement.fall_distance = 0.0;
                    p.view.pending_teleport = true;
                    out.push(GameOutput::PlayerTeleported {
                        uuid,
                        position: spawn,
                        yaw: 0.0,
                        pitch: 0.0,
                    });
                    continue;
                }

                let old_pos = p.pos.0;
                let was_on_ground = p.movement.on_ground;
                if let Some(pos) = position {
                    p.pos.0 = pos;
                }
                if let Some([yaw, pitch]) = rotation {
                    p.rot.yaw = yaw;
                    p.rot.pitch = pitch;
                }
                p.movement.on_ground = on_ground;

                let is_survival = p.profile.is_survival();
                let is_dead = p.dead.is_some() || p.health.current <= 0.0;
                let mut death_reason: Option<&'static str> = None;

                if is_survival && !is_dead {
                    let [old_x, old_y, old_z] = old_pos;
                    let [px, py, pz] = p.pos.0;
                    let horiz = ((px - old_x).powi(2) + (pz - old_z).powi(2)).sqrt();
                    if p.movement.sprinting && horiz > 0.0 && horiz < 10.0 {
                        add_exhaustion(&mut p.hunger, true, false, 0.1 * horiz as f32);
                    }
                    if was_on_ground && !on_ground && py > old_y {
                        add_exhaustion(
                            &mut p.hunger,
                            true,
                            false,
                            if p.movement.sprinting { 0.2 } else { 0.05 },
                        );
                    }

                    if py < f64::from(world::MIN_Y) - 64.0 {
                        let (damaged, died) = apply_damage_at_tick(
                            &mut p.health,
                            &mut p.movement,
                            true,
                            false,
                            4.0,
                            tick,
                        );
                        if damaged {
                            out.push(GameOutput::PlayerHurt {
                                uuid,
                                entity_id: p.net_id.0,
                            });
                            out.push(GameOutput::PlayerVitalsUpdated {
                                uuid,
                                health: p.health.current,
                                food: p.hunger.food,
                                saturation: p.hunger.saturation,
                            });
                            if died {
                                death_reason = Some("fell out of the world");
                            }
                        }
                    } else {
                        let feet_pos =
                            BlockPos([px.floor() as i32, py.floor() as i32, pz.floor() as i32]);
                        let in_liquid = world
                            .0
                            .block_at_if_loaded(feet_pos)
                            .and_then(crate::game::block_by_state)
                            .is_some_and(|d| d.liquid);
                        if in_liquid {
                            p.movement.fall_distance = 0.0;
                        } else if on_ground {
                            let fell = std::mem::take(&mut p.movement.fall_distance);
                            if fell > 3.0 {
                                let damage = (fell - 3.0).ceil() as f32;
                                let (damaged, died) = apply_damage_at_tick(
                                    &mut p.health,
                                    &mut p.movement,
                                    true,
                                    false,
                                    damage,
                                    tick,
                                );
                                if damaged {
                                    out.push(GameOutput::PlayerHurt {
                                        uuid,
                                        entity_id: p.net_id.0,
                                    });
                                    out.push(GameOutput::PlayerVitalsUpdated {
                                        uuid,
                                        health: p.health.current,
                                        food: p.hunger.food,
                                        saturation: p.hunger.saturation,
                                    });
                                    if died {
                                        death_reason = Some("fell from a high place");
                                    }
                                }
                            }
                        } else if py < old_y {
                            p.movement.fall_distance += old_y - py;
                        }
                    }
                }

                let pose = entity_pose(*p.pos, *p.rot, *p.movement);
                out.push(GameOutput::PlayerPoseUpdated { uuid, pose });

                if let Some(reason) = death_reason {
                    commands.entity(entity).insert(Dead);
                    let dropped = p.inv.0.drain_all();
                    let snapshot = p.inv.0.clone();
                    let main_hand = p.inv.0.hand(Hand::Main);
                    let off_hand = p.inv.0.hand(Hand::Off);
                    let drop_pos = [p.pos.0[0], p.pos.0[1] + 0.5, p.pos.0[2]];
                    for stack in dropped {
                        drops.write(SpawnDropMsg::scatter(drop_pos, stack, 40));
                    }
                    out.push(GameOutput::PlayerInventorySynced {
                        uuid,
                        inventory: snapshot,
                    });
                    out.push(GameOutput::PlayerEquipmentChanged {
                        uuid,
                        entity_id: p.net_id.0,
                        main_hand,
                        off_hand,
                    });
                    out.push(GameOutput::BroadcastSystemChat(format!(
                        "{} {reason}",
                        p.profile.name
                    )));
                }
            }
            MovementMsg::EntityAction { uuid, action } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok(mut p) = players.get_mut(entity) else {
                    continue;
                };
                let mut changed = false;
                match action {
                    0 if !p.movement.sneaking => {
                        p.movement.sneaking = true;
                        changed = true;
                    }
                    1 if p.movement.sneaking => {
                        p.movement.sneaking = false;
                        changed = true;
                    }
                    3 if !p.movement.sprinting => {
                        p.movement.sprinting = true;
                        changed = true;
                    }
                    4 if p.movement.sprinting => {
                        p.movement.sprinting = false;
                        changed = true;
                    }
                    _ => {}
                }
                if changed {
                    out.push(GameOutput::PlayerMetadataChanged {
                        uuid,
                        entity_id: p.net_id.0,
                        sneaking: p.movement.sneaking,
                        sprinting: p.movement.sprinting,
                    });
                }
            }
            MovementMsg::ArmAnimation { uuid, hand } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                if let Ok(p) = players.get(entity) {
                    out.push(GameOutput::PlayerArmSwing {
                        uuid,
                        entity_id: p.net_id.0,
                        hand,
                    });
                }
            }
            MovementMsg::Respawn { uuid } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok(mut p) = players.get_mut(entity) else {
                    continue;
                };
                if p.dead.is_none() {
                    continue;
                }

                let world_spawn = world.0.spawn_position();
                let mut missing_bed = false;
                let spawn = if let Some(bed_pos) = p.profile.respawn_point {
                    let is_bed = bed_pos.valid()
                        && crate::game::registry()
                            .state_by_id(world.0.block_at(bed_pos))
                            .is_some_and(|(_, def, _)| def.name.ends_with("_bed"));
                    if is_bed {
                        [
                            f64::from(bed_pos.0[0]) + 0.5,
                            f64::from(bed_pos.0[1]) + 0.6,
                            f64::from(bed_pos.0[2]) + 0.5,
                        ]
                    } else {
                        missing_bed = true;
                        world_spawn
                    }
                } else {
                    world_spawn
                };

                if missing_bed {
                    p.profile.respawn_point = None;
                }
                p.pos.0 = spawn;
                *p.rot = Rotation::default();
                *p.movement = MovementState::default();
                *p.health = Health::default();
                *p.hunger = Hunger::default();
                p.container.open = None;
                p.view.tracked_chunks.clear();
                p.view.pending_teleport = true;
                commands
                    .entity(entity)
                    .remove::<(Dead, EatingState, MiningState)>();

                let pose = entity_pose(*p.pos, *p.rot, *p.movement);
                out.push(GameOutput::PlayerPoseUpdated { uuid, pose });
                out.push(GameOutput::PlayerRespawned {
                    uuid,
                    spawn,
                    game_mode: p.profile.game_mode,
                    health: p.health.current,
                    food: p.hunger.food,
                    saturation: p.hunger.saturation,
                    missing_bed,
                    inventory: p.inv.0.clone(),
                });
            }
        }
    }
}
