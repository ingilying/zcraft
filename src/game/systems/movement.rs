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
use bevy_ecs::prelude::{Commands, MessageReader, MessageWriter, Query, Res, ResMut, With};

/// Process player movement, posture flags, arm animation, and respawn requests.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn player_movement_system(
    mut messages: MessageReader<MovementMsg>,
    clock: Res<TickClock>,
    world: Res<VoxelWorld>,
    registry: Res<EntityRegistry>,
    mut players: Query<
        (
            &NetworkEntityId,
            &mut PlayerProfile,
            &mut Position,
            &mut Rotation,
            &mut MovementState,
            &mut Health,
            &mut Hunger,
            Option<&Dead>,
            &mut PlayerInventory,
            &mut PlayerContainer,
            &mut PlayerViewState,
        ),
        With<PlayerMarker>,
    >,
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
                let Ok((
                    net_id,
                    profile,
                    mut pos,
                    mut rot,
                    mut movement,
                    mut health,
                    mut hunger,
                    dead,
                    mut inv,
                    _,
                    mut view,
                )) = players.get_mut(entity)
                else {
                    continue;
                };
                if view.pending_teleport {
                    continue;
                }

                if let Some(target_pos) = position
                    && !Player::position_in_bounds(target_pos)
                {
                    let spawn = world.0.spawn_position();
                    pos.0 = spawn;
                    *rot = Rotation::default();
                    movement.on_ground = false;
                    movement.fall_distance = 0.0;
                    view.pending_teleport = true;
                    out.push(GameOutput::PlayerTeleported {
                        uuid,
                        position: spawn,
                        yaw: 0.0,
                        pitch: 0.0,
                    });
                    continue;
                }

                let old_pos = pos.0;
                let was_on_ground = movement.on_ground;
                if let Some(p) = position {
                    pos.0 = p;
                }
                if let Some([yaw, pitch]) = rotation {
                    rot.yaw = yaw;
                    rot.pitch = pitch;
                }
                movement.on_ground = on_ground;

                let is_survival = profile.is_survival();
                let is_dead = dead.is_some() || health.current <= 0.0;
                let mut death_reason: Option<&'static str> = None;

                if is_survival && !is_dead {
                    let [old_x, old_y, old_z] = old_pos;
                    let [px, py, pz] = pos.0;
                    let horiz = ((px - old_x).powi(2) + (pz - old_z).powi(2)).sqrt();
                    if movement.sprinting && horiz > 0.0 && horiz < 10.0 {
                        add_exhaustion(&mut hunger, true, false, 0.1 * horiz as f32);
                    }
                    if was_on_ground && !on_ground && py > old_y {
                        add_exhaustion(
                            &mut hunger,
                            true,
                            false,
                            if movement.sprinting { 0.2 } else { 0.05 },
                        );
                    }

                    if py < f64::from(world::MIN_Y) - 64.0 {
                        let (damaged, died) = apply_damage_at_tick(
                            &mut health,
                            &mut movement,
                            true,
                            false,
                            4.0,
                            tick,
                        );
                        if damaged {
                            out.push(GameOutput::PlayerHurt {
                                uuid,
                                entity_id: net_id.0,
                            });
                            out.push(GameOutput::PlayerVitalsUpdated {
                                uuid,
                                health: health.current,
                                food: hunger.food,
                                saturation: hunger.saturation,
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
                            movement.fall_distance = 0.0;
                        } else if on_ground {
                            let fell = std::mem::take(&mut movement.fall_distance);
                            if fell > 3.0 {
                                let damage = (fell - 3.0).ceil() as f32;
                                let (damaged, died) = apply_damage_at_tick(
                                    &mut health,
                                    &mut movement,
                                    true,
                                    false,
                                    damage,
                                    tick,
                                );
                                if damaged {
                                    out.push(GameOutput::PlayerHurt {
                                        uuid,
                                        entity_id: net_id.0,
                                    });
                                    out.push(GameOutput::PlayerVitalsUpdated {
                                        uuid,
                                        health: health.current,
                                        food: hunger.food,
                                        saturation: hunger.saturation,
                                    });
                                    if died {
                                        death_reason = Some("fell from a high place");
                                    }
                                }
                            }
                        } else if py < old_y {
                            movement.fall_distance += old_y - py;
                        }
                    }
                }

                let pose = entity_pose(*pos, *rot, *movement);
                out.push(GameOutput::PlayerPoseUpdated { uuid, pose });

                if let Some(reason) = death_reason {
                    commands.entity(entity).insert(Dead);
                    let dropped = inv.0.drain_all();
                    let snapshot = inv.0.clone();
                    let main_hand = inv.0.hand(Hand::Main);
                    let off_hand = inv.0.hand(Hand::Off);
                    let drop_pos = [pos.0[0], pos.0[1] + 0.5, pos.0[2]];
                    for stack in dropped {
                        drops.write(SpawnDropMsg::scatter(drop_pos, stack, 40));
                    }
                    out.push(GameOutput::PlayerInventorySynced {
                        uuid,
                        inventory: snapshot,
                    });
                    out.push(GameOutput::PlayerEquipmentChanged {
                        uuid,
                        entity_id: net_id.0,
                        main_hand,
                        off_hand,
                    });
                    out.push(GameOutput::BroadcastSystemChat(format!(
                        "{} {reason}",
                        profile.name
                    )));
                }
            }
            MovementMsg::EntityAction { uuid, action } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok((net_id, _, _, _, mut movement, ..)) = players.get_mut(entity) else {
                    continue;
                };
                let mut changed = false;
                match action {
                    0 if !movement.sneaking => {
                        movement.sneaking = true;
                        changed = true;
                    }
                    1 if movement.sneaking => {
                        movement.sneaking = false;
                        changed = true;
                    }
                    3 if !movement.sprinting => {
                        movement.sprinting = true;
                        changed = true;
                    }
                    4 if movement.sprinting => {
                        movement.sprinting = false;
                        changed = true;
                    }
                    _ => {}
                }
                if changed {
                    out.push(GameOutput::PlayerMetadataChanged {
                        uuid,
                        entity_id: net_id.0,
                        sneaking: movement.sneaking,
                        sprinting: movement.sprinting,
                    });
                }
            }
            MovementMsg::ArmAnimation { uuid, hand } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                if let Ok((net_id, ..)) = players.get(entity) {
                    out.push(GameOutput::PlayerArmSwing {
                        uuid,
                        entity_id: net_id.0,
                        hand,
                    });
                }
            }
            MovementMsg::Respawn { uuid } => {
                let Some(&entity) = registry.players.get(&uuid) else {
                    continue;
                };
                let Ok((
                    _,
                    mut profile,
                    mut pos,
                    mut rot,
                    mut movement,
                    mut health,
                    mut hunger,
                    dead,
                    inv,
                    mut container,
                    mut view,
                )) = players.get_mut(entity)
                else {
                    continue;
                };
                if dead.is_none() {
                    continue;
                }

                let world_spawn = world.0.spawn_position();
                let mut missing_bed = false;
                let spawn = if let Some(bed_pos) = profile.respawn_point {
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
                    profile.respawn_point = None;
                }
                pos.0 = spawn;
                *rot = Rotation::default();
                *movement = MovementState::default();
                *health = Health::default();
                *hunger = Hunger::default();
                container.open = None;
                view.tracked_chunks.clear();
                view.pending_teleport = true;
                commands
                    .entity(entity)
                    .remove::<(Dead, EatingState, MiningState)>();

                let pose = entity_pose(*pos, *rot, *movement);
                out.push(GameOutput::PlayerPoseUpdated { uuid, pose });
                out.push(GameOutput::PlayerRespawned {
                    uuid,
                    spawn,
                    game_mode: profile.game_mode,
                    health: health.current,
                    food: hunger.food,
                    saturation: hunger.saturation,
                    missing_bed,
                    inventory: inv.0.clone(),
                });
            }
        }
    }
}
