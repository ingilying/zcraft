//! Network IO state-synchronization bridge: translates protocol-agnostic
//! `GameOutput` events from the ECS Game Engine into protocol 767 `Clientbound`
//! packets and routes them through `Players`.
use crate::{
    config::GameMode,
    game::{ContainerOpenSync, GameOutput},
    players::{PlayerControl, Players},
    protocol::{
        Clientbound,
        packets::{
            play::{
                BlockDestruction, GameEvent, HurtAnimation, PlayerAbilities, TimeUpdate,
                UpdateHealth,
            },
            play_blocks::{
                BlockAck, BlockAction, BlockUpdate, CloseContainer, ContainerSetData,
                InventoryContent, LightUpdate, OpenScreen, SelectedSlot, SetInventorySlot,
            },
            play_chat::SystemChat,
            play_entities::{
                CollectItem, EntityAnimation, EntityEquipment, EntityMetadata, EntityPose,
                EntityTeleport, EntityVelocity, RemoveEntities, SpawnEntity,
            },
        },
    },
    world::{BlockPos, TimeSnapshot, WeatherEvent, WorldMutation},
};
use std::{collections::BTreeMap, io, sync::Arc};

pub(super) fn encode_time(time: TimeSnapshot) -> TimeUpdate {
    TimeUpdate {
        world_age: time.world_age,
        time_of_day: time.day_time,
    }
}

pub(super) fn encode_weather(event: WeatherEvent) -> Clientbound {
    match event {
        WeatherEvent::StartRaining => GameEvent::start_raining().into(),
        WeatherEvent::StopRaining => GameEvent::stop_raining().into(),
        WeatherEvent::RainLevel(level) => GameEvent::rain_level(level).into(),
        WeatherEvent::ThunderLevel(level) => GameEvent::thunder_level(level).into(),
    }
}

pub(super) fn furnace_property_packets(window_id: u8, props: [i16; 4]) -> [Clientbound; 4] {
    [
        ContainerSetData {
            window_id,
            property: 0,
            value: props[0],
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 1,
            value: props[1],
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 2,
            value: props[2],
        }
        .into(),
        ContainerSetData {
            window_id,
            property: 3,
            value: props[3],
        }
        .into(),
    ]
}

fn encode_chunk_updates(
    chunk: [i32; 2],
    changes: Vec<(BlockPos, i32)>,
    light_update: Option<Arc<[u8]>>,
) -> io::Result<Arc<[Arc<[u8]>]>> {
    if changes.is_empty() && light_update.is_none() {
        return Ok(Arc::from([]));
    }
    let mut bodies = Vec::with_capacity(changes.len() + usize::from(light_update.is_some()));
    for (pos, state) in changes {
        bodies.push(Clientbound::from(BlockUpdate(pos, state)).encode()?.into());
    }
    if let Some(light) = light_update {
        bodies.push(
            Clientbound::from(LightUpdate(chunk, light))
                .encode()?
                .into(),
        );
    }
    Ok(bodies.into())
}

pub(super) fn publish_mutation(players: &Players, mutation: WorldMutation) -> io::Result<()> {
    let mut per_chunk: BTreeMap<[i32; 2], Vec<Arc<[u8]>>> = BTreeMap::new();
    for (pos, state) in mutation.block_changes {
        let body = Clientbound::from(BlockUpdate(pos, state)).encode()?.into();
        per_chunk.entry(pos.chunk()).or_default().push(body);
    }
    for (chunk, light) in mutation.light_changes {
        let body = Clientbound::from(LightUpdate(chunk, light))
            .encode()?
            .into();
        per_chunk.entry(chunk).or_default().push(body);
    }
    for (chunk, bodies) in per_chunk {
        players.publish_world_updates(chunk, bodies.into());
    }
    Ok(())
}

fn broadcast_chest_lid(players: &Players, pos: BlockPos, viewer_count: usize) -> io::Result<()> {
    let Some(chest_block_id) = crate::game::registry()
        .block_by_name("chest")
        .map(|(id, _)| id as i32)
    else {
        return Ok(());
    };
    let body: Arc<[u8]> = Clientbound::from(BlockAction {
        pos,
        action_id: 1,
        action_param: viewer_count.min(255) as u8,
        block_id: chest_block_id,
    })
    .encode()?
    .into();
    players.publish_world_updates(pos.chunk(), Arc::from([body]));
    Ok(())
}

/// Apply a batch of ECS `GameOutput` events to the network `Players` fanout layer.
/// Any `PersistPlayer` events are delegated to `on_persist` so the dedicated game-loop
/// thread never blocks on playerdata disk I/O.
pub(crate) fn apply_outputs(
    players: &Players,
    outputs: Vec<GameOutput>,
    mut on_persist: impl FnMut(Box<crate::game::Player>, Box<crate::game::Inventory>),
) -> io::Result<()> {
    for output in outputs {
        match output {
            GameOutput::TimeUpdated(time) => {
                let _ = players.broadcast(&encode_time(time).into());
            }
            GameOutput::WeatherChanged(event) => {
                let _ = players.broadcast(&encode_weather(event));
            }
            GameOutput::WorldMutated(mutation) => {
                publish_mutation(players, mutation)?;
            }
            GameOutput::WorldMutatedWithAck {
                actor,
                chunk,
                mutation,
                sequence,
            } => {
                let mut center_light = None;
                for (nc, light) in mutation.light_changes {
                    if nc == chunk {
                        center_light = Some(light);
                    } else {
                        let body = Clientbound::from(LightUpdate(nc, light)).encode()?.into();
                        players.publish_light_chunk(nc, body);
                    }
                }
                let data = encode_chunk_updates(chunk, mutation.block_changes, center_light)?;
                let ack: Arc<[u8]> = Clientbound::from(BlockAck(sequence)).encode()?.into();
                players.publish_blocks_for(actor, chunk, data, ack);
            }
            GameOutput::BlockCorrected {
                player,
                corrections,
                sequence,
            } => {
                let mut encoded = Vec::with_capacity(corrections.len());
                for (pos, state) in corrections {
                    let body = Clientbound::from(BlockUpdate(pos, state)).encode()?.into();
                    encoded.push((pos.chunk(), body));
                }
                let ack: Arc<[u8]> = Clientbound::from(BlockAck(sequence)).encode()?.into();
                players.correct_blocks_for(player, encoded, ack);
            }
            GameOutput::BlockAckOnly { player, sequence } => {
                let ack: Arc<[u8]> = Clientbound::from(BlockAck(sequence)).encode()?.into();
                players.correct_blocks_for(player, Vec::new(), ack);
            }
            GameOutput::ChestLidUpdated { pos, viewer_count } => {
                broadcast_chest_lid(players, pos, viewer_count)?;
            }
            GameOutput::BroadcastSystemChat(msg) => {
                let _ = players.broadcast_system_chat(&msg);
            }
            GameOutput::ItemDropSpawned {
                id,
                uuid,
                position,
                velocity,
                stack,
            } => {
                let _ = players.broadcast_entity_packet(
                    &SpawnEntity::item(id, uuid, position, velocity).into(),
                );
                let _ = players.broadcast_entity_packet(&EntityMetadata::Item { id, stack }.into());
                let _ = players.broadcast_entity_packet(&EntityVelocity::new(id, velocity).into());
            }
            GameOutput::ItemDropUpdated { id, stack } => {
                let _ = players.broadcast_entity_packet(&EntityMetadata::Item { id, stack }.into());
            }
            GameOutput::ItemDropMoved {
                id,
                position,
                velocity,
            } => {
                let _ = players.broadcast_entity_packet(
                    &EntityTeleport {
                        id,
                        pose: EntityPose::new(position, 0.0, 0.0, false),
                    }
                    .into(),
                );
                let _ = players.broadcast_entity_packet(&EntityVelocity::new(id, velocity).into());
            }
            GameOutput::ItemDropCollected {
                drop_id,
                collector_id,
                count,
            } => {
                let _ = players.broadcast_entity_packet(
                    &CollectItem {
                        collected_id: drop_id,
                        collector_id,
                        count: i32::from(count),
                    }
                    .into(),
                );
            }
            GameOutput::EntitiesRemoved(ids) => {
                if !ids.is_empty() {
                    let _ = players.broadcast_entity_packet(&RemoveEntities(ids).into());
                }
            }
            GameOutput::BlockDestruction {
                entity_id,
                pos,
                stage,
            } => {
                let _ = players.broadcast_entity_packet(
                    &BlockDestruction {
                        entity_id,
                        pos,
                        stage,
                    }
                    .into(),
                );
            }
            GameOutput::PlayerPoseUpdated { uuid, pose } => {
                players.update_pose_for(uuid, pose);
            }
            GameOutput::PlayerTeleported {
                uuid,
                position,
                yaw,
                pitch,
            } => {
                players.send_control(
                    uuid,
                    PlayerControl::SyncTeleport {
                        position,
                        yaw,
                        pitch,
                    },
                );
            }
            GameOutput::PlayerRespawned {
                uuid,
                spawn,
                game_mode,
                health,
                food,
                saturation,
                missing_bed,
                inventory,
            } => {
                players.send_control(
                    uuid,
                    PlayerControl::SyncRespawn {
                        spawn,
                        game_mode,
                        health,
                        food,
                        saturation,
                        missing_bed,
                        inventory: Box::new(inventory),
                    },
                );
            }
            GameOutput::PlayerGameModeChanged {
                uuid,
                mode,
                health,
                food,
                saturation,
                inventory,
            } => {
                players.update_game_mode_for(uuid, mode);
                let _ = players.send_packet(uuid, &GameEvent::change_game_mode(mode).into());
                let _ = players.send_packet(uuid, &PlayerAbilities(mode).into());
                if mode == GameMode::Survival {
                    let _ = players
                        .send_packet(uuid, &UpdateHealth::new(health, food, saturation).into());
                }
                if mode != GameMode::Spectator {
                    let _ = players.send_packet(uuid, &InventoryContent::player(&inventory).into());
                    let _ = players.send_packet(uuid, &SelectedSlot(inventory.selected()).into());
                }
            }
            GameOutput::PlayerVitalsUpdated {
                uuid,
                health,
                food,
                saturation,
            } => {
                let _ =
                    players.send_packet(uuid, &UpdateHealth::new(health, food, saturation).into());
            }
            GameOutput::PlayerHurt { uuid, entity_id } => {
                let hurt = Clientbound::from(HurtAnimation::new(entity_id, 0.0));
                let _ = players.broadcast_to_viewers_of(uuid, &hurt);
                let _ = players.send_packet(uuid, &hurt);
            }
            GameOutput::PlayerMetadataChanged {
                uuid,
                entity_id,
                sneaking,
                sprinting,
            } => {
                let packet = Clientbound::from(EntityMetadata::Player {
                    id: entity_id,
                    sneaking,
                    sprinting,
                });
                let _ = players.broadcast_to_viewers_of(uuid, &packet);
            }
            GameOutput::PlayerArmSwing {
                uuid,
                entity_id,
                hand,
            } => {
                let animation = if hand == 0 { 0 } else { 3 };
                let packet = Clientbound::from(EntityAnimation {
                    id: entity_id,
                    animation,
                });
                let _ = players.broadcast_to_viewers_of(uuid, &packet);
            }
            GameOutput::PlayerEquipmentChanged {
                uuid,
                entity_id,
                main_hand,
                off_hand,
            } => {
                let packet = Clientbound::from(EntityEquipment {
                    id: entity_id,
                    main_hand,
                    off_hand,
                });
                let _ = players.broadcast_to_viewers_of(uuid, &packet);
            }
            GameOutput::PlayerInventorySynced { uuid, inventory } => {
                let _ = players.send_packet(uuid, &InventoryContent::player(&inventory).into());
            }
            GameOutput::PlayerSlotSynced {
                uuid,
                state_id,
                slot,
                item,
            } => {
                let _ = players.send_packet(
                    uuid,
                    &SetInventorySlot {
                        state_id,
                        slot,
                        item,
                    }
                    .into(),
                );
            }
            GameOutput::PlayerSystemMessage { uuid, message } => {
                let _ = players.send_packet(uuid, &SystemChat(message).into());
            }
            GameOutput::ContainerOpened {
                uuid,
                window_id,
                screen,
            } => match screen {
                ContainerOpenSync::CraftingTable { slots, carried } => {
                    let _ = players.send_packet(
                        uuid,
                        &OpenScreen {
                            window_id: i32::from(window_id),
                            menu_type: 12,
                            title: "Crafting".into(),
                        }
                        .into(),
                    );
                    let _ = players.send_packet(
                        uuid,
                        &InventoryContent::container(window_id, 0, slots, carried).into(),
                    );
                }
                ContainerOpenSync::Chest { slots, carried } => {
                    let _ = players.send_packet(
                        uuid,
                        &OpenScreen {
                            window_id: i32::from(window_id),
                            menu_type: 2,
                            title: "Chest".into(),
                        }
                        .into(),
                    );
                    let _ = players.send_packet(
                        uuid,
                        &InventoryContent::container(window_id, 0, slots, carried).into(),
                    );
                }
                ContainerOpenSync::Furnace {
                    slots,
                    carried,
                    properties,
                } => {
                    let _ = players.send_packet(
                        uuid,
                        &OpenScreen {
                            window_id: i32::from(window_id),
                            menu_type: 14,
                            title: "Furnace".into(),
                        }
                        .into(),
                    );
                    let _ = players.send_packet(
                        uuid,
                        &InventoryContent::container(window_id, 0, slots, carried).into(),
                    );
                    for prop_pkt in furnace_property_packets(window_id, properties) {
                        let _ = players.send_packet(uuid, &prop_pkt);
                    }
                }
            },
            GameOutput::ContainerSynced {
                uuid,
                window_id,
                state_id,
                slots,
                carried,
                furnace_props,
            } => {
                let _ = players.send_packet(
                    uuid,
                    &InventoryContent::container(window_id, state_id, slots, carried).into(),
                );
                if let Some(props) = furnace_props {
                    for prop_pkt in furnace_property_packets(window_id, props) {
                        let _ = players.send_packet(uuid, &prop_pkt);
                    }
                }
            }
            GameOutput::ContainerClosed { uuid, window_id } => {
                let _ = players.send_packet(uuid, &CloseContainer(window_id).into());
            }
            GameOutput::PersistPlayer { player, inventory } => {
                on_persist(player, inventory);
            }
        }
    }
    Ok(())
}
