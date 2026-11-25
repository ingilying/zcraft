//! Shared Play-phase service: coordinates the World domain service, OP list,
//! command registry, entity IDs, and graceful shutdown trigger.
use crate::{
    config::GameMode,
    protocol::{
        Clientbound,
        packets::{
            play::{GameEvent, TimeUpdate},
            play_blocks::{BlockUpdate, LightUpdate},
            play_entities::{
                CollectItem, EntityMetadata, EntityVelocity, RemoveEntities, SpawnEntity,
            },
        },
    },
    world::{self, TimeSnapshot, WeatherEvent, World, WorldMutation},
};
use std::{
    collections::{BTreeMap, HashSet},
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI32, Ordering},
    },
};

const MAX_ITEM_DROPS: usize = 512;

#[derive(Debug, Clone)]
struct ItemDrop {
    id: i32,
    position: [f64; 3],
    velocity: [f64; 3],
    stack: crate::game::ItemStack,
    pickup_delay: u16,
    age: u16,
}

pub struct PlayService {
    world: Arc<World>,
    drops: Mutex<Vec<ItemDrop>>,
    ops: Mutex<HashSet<String>>,
    shutdown: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    pub(super) game_mode: GameMode,
    commands: crate::command::CommandRegistry,
    max_view_distance: i32,
    next_entity: AtomicI32,
}

impl PlayService {
    pub fn new() -> io::Result<Self> {
        Self::with_max_view_distance(world::DEFAULT_VIEW_DISTANCE)
    }

    pub fn with_max_view_distance(max_view_distance: i32) -> io::Result<Self> {
        Self::with_options(max_view_distance, GameMode::Spectator)
    }

    pub fn with_options(max_view_distance: i32, game_mode: GameMode) -> io::Result<Self> {
        Self::with_world(max_view_distance, game_mode, Arc::new(World::new()))
    }

    pub fn with_world(
        max_view_distance: i32,
        game_mode: GameMode,
        world: Arc<World>,
    ) -> io::Result<Self> {
        world::validate_view_distance(max_view_distance)?;
        Ok(Self {
            world,
            drops: Mutex::new(Vec::new()),
            ops: Mutex::new(HashSet::new()),
            shutdown: Mutex::new(None),
            game_mode,
            commands: crate::command::CommandRegistry::default(),
            max_view_distance,
            next_entity: AtomicI32::new(1),
        })
    }

    pub fn world(&self) -> &Arc<World> {
        &self.world
    }

    pub fn save_world(&self) -> io::Result<world::SaveSummary> {
        self.world.save_all()
    }

    pub fn load_player(&self, uuid: uuid::Uuid) -> io::Result<Option<world::SavedPlayer>> {
        self.world.load_player(uuid)
    }

    pub fn save_player(
        &self,
        player: &crate::game::Player,
        inventory: &crate::game::Inventory,
    ) -> io::Result<()> {
        self.world.save_player(player, inventory)
    }

    pub(super) fn allocate_entity_id(&self) -> io::Result<i32> {
        self.next_entity
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| io::Error::other("entity ID space exhausted"))
    }

    pub fn with_ops(self, ops: impl IntoIterator<Item = String>) -> Self {
        {
            let mut set = self.ops.lock().expect("ops poisoned");
            for entry in ops {
                set.insert(normalize_op(&entry));
            }
        }
        self
    }

    pub fn permission_for(&self, uuid: uuid::Uuid, username: &str) -> u8 {
        let ops = self.ops.lock().expect("ops poisoned");
        if ops.contains(&uuid.to_string()) || ops.contains(&username.to_ascii_lowercase()) {
            4
        } else {
            0
        }
    }

    pub fn add_op(&self, target: &str) -> bool {
        self.ops
            .lock()
            .expect("ops poisoned")
            .insert(normalize_op(target))
    }

    pub fn remove_op(&self, target: &str) -> bool {
        self.ops
            .lock()
            .expect("ops poisoned")
            .remove(&normalize_op(target))
    }

    pub fn set_shutdown_trigger(&self, tx: tokio::sync::watch::Sender<bool>) {
        *self.shutdown.lock().expect("shutdown poisoned") = Some(tx);
    }

    pub fn request_shutdown(&self) -> bool {
        if let Some(tx) = self.shutdown.lock().expect("shutdown poisoned").as_ref() {
            let _ = tx.send(true);
            true
        } else {
            false
        }
    }

    pub fn set_block(
        &self,
        players: &crate::players::Players,
        pos: world::BlockPos,
        state: i32,
    ) -> io::Result<bool> {
        let Some(mutation) = self.world.set_block(pos, state) else {
            return Ok(false);
        };
        publish_mutation(players, mutation)?;
        Ok(true)
    }

    pub fn fill_blocks(
        &self,
        players: &crate::players::Players,
        from: world::BlockPos,
        to: world::BlockPos,
        state: i32,
    ) -> io::Result<usize> {
        let mutation = self.world.fill_blocks(from, to, state)?;
        let count = mutation.block_changes.len();
        publish_mutation(players, mutation)?;
        Ok(count)
    }

    /// Spawn a dropped `ItemEntity` in the world and broadcast it to players.
    pub fn spawn_item_drop(
        &self,
        players: &crate::players::Players,
        position: [f64; 3],
        velocity: [f64; 3],
        stack: crate::game::ItemStack,
        pickup_delay: u16,
    ) -> io::Result<()> {
        let id = self.allocate_entity_id()?;
        let uuid = uuid::Uuid::from_u128(rand::random());
        let mut evicted = None;
        {
            let mut drops = self.drops.lock().expect("item drops poisoned");
            if drops.len() >= MAX_ITEM_DROPS {
                evicted = Some(drops.remove(0).id);
            }
            drops.push(ItemDrop {
                id,
                position,
                velocity,
                stack,
                pickup_delay,
                age: 0,
            });
        }
        if let Some(old_id) = evicted {
            let _ = players.broadcast_entity_packet(&RemoveEntities(vec![old_id]).into());
        }
        let _ = players
            .broadcast_entity_packet(&SpawnEntity::item(id, uuid, position, velocity).into());
        let _ = players.broadcast_entity_packet(&EntityMetadata::Item { id, stack }.into());
        let _ = players.broadcast_entity_packet(&EntityVelocity::new(id, velocity).into());
        Ok(())
    }

    /// Advance dropped item physics, expiration, and proximity pickup (20 TPS).
    pub fn tick_drops(&self, players: &crate::players::Players) {
        let candidates = players.pickup_candidates();
        let mut collected: Vec<(uuid::Uuid, i32, i32, crate::game::ItemStack)> = Vec::new();
        let mut expired: Vec<i32> = Vec::new();
        {
            let mut drops = self.drops.lock().expect("item drops poisoned");
            drops.retain_mut(|drop| {
                drop.age = drop.age.saturating_add(1);
                if drop.age >= 6000 || drop.position[1] < f64::from(world::MIN_Y) - 10.0 {
                    expired.push(drop.id);
                    return false;
                }
                drop.pickup_delay = drop.pickup_delay.saturating_sub(1);

                // Simple gravity + ground stop.
                drop.velocity[1] = (drop.velocity[1] - 0.04).max(-1.0);
                for axis in 0..3 {
                    drop.position[axis] += drop.velocity[axis];
                    drop.velocity[axis] *= 0.98;
                }
                let below = world::BlockPos([
                    drop.position[0].floor() as i32,
                    (drop.position[1] - 0.1).floor() as i32,
                    drop.position[2].floor() as i32,
                ]);
                if below.valid()
                    && crate::game::block_by_state(self.world.block_at(below))
                        .is_some_and(|d| d.collision)
                {
                    drop.position[1] = f64::from(below.0[1] + 1) + 0.125;
                    drop.velocity[1] = 0.0;
                    drop.velocity[0] *= 0.6;
                    drop.velocity[2] *= 0.6;
                }

                if drop.pickup_delay == 0 {
                    for &(uuid, player_id, ppos) in &candidates {
                        let dx = ppos[0] - drop.position[0];
                        let dy = (ppos[1] + 0.9) - drop.position[1];
                        let dz = ppos[2] - drop.position[2];
                        if dx * dx + dy * dy + dz * dz <= 2.25 {
                            collected.push((uuid, drop.id, player_id, drop.stack));
                            return false;
                        }
                    }
                }
                true
            });
        }
        if !expired.is_empty() {
            let _ = players.broadcast_entity_packet(&RemoveEntities(expired).into());
        }
        for (uuid, drop_id, player_id, stack) in collected {
            players.send_control(
                uuid,
                crate::players::PlayerControl::Give {
                    item: stack.item,
                    count: u32::from(stack.count),
                },
            );
            let _ = players.broadcast_entity_packet(
                &CollectItem {
                    collected_id: drop_id,
                    collector_id: player_id,
                    count: i32::from(stack.count),
                }
                .into(),
            );
            let _ = players.broadcast_entity_packet(&RemoveEntities(vec![drop_id]).into());
        }
    }

    /// Advance the world clock and weather cycle by one 20 TPS tick and map
    /// domain events to protocol 767 packets.
    pub fn tick_world(&self) -> Vec<Clientbound> {
        let events = self.world.tick();
        let mut packets = Vec::with_capacity(events.weather.len() + 1);
        if let Some(time) = events.time_sync {
            packets.push(encode_time(time).into());
        }
        packets.extend(events.weather.into_iter().map(encode_weather));
        packets
    }

    pub fn world_time(&self) -> (i64, i64) {
        let time = self.world.time();
        (time.world_age, time.day_time)
    }

    pub fn weather(&self) -> world::WeatherSnapshot {
        self.world.weather()
    }

    pub fn set_day_time(&self, day_time: i64) -> TimeUpdate {
        encode_time(self.world.set_day_time(day_time))
    }

    pub fn set_weather(&self, raining: bool, thundering: bool, duration: u32) -> Vec<Clientbound> {
        self.world
            .set_weather(raining, thundering, duration)
            .into_iter()
            .map(encode_weather)
            .collect()
    }

    pub(super) fn initial_environment_packets(&self) -> Vec<Clientbound> {
        let (time, weather) = self.world.initial_environment();
        let mut packets = Vec::with_capacity(weather.len() + 1);
        packets.push(encode_time(time).into());
        packets.extend(weather.into_iter().map(encode_weather));
        packets
    }

    /// Install a validated immutable registry before serving connections.
    pub fn with_commands(mut self, commands: crate::command::CommandRegistry) -> Self {
        self.commands = commands;
        self
    }

    pub fn commands(&self) -> &crate::command::CommandRegistry {
        &self.commands
    }

    pub fn game_mode(&self) -> GameMode {
        self.game_mode
    }

    pub fn max_view_distance(&self) -> i32 {
        self.max_view_distance
    }

    pub(super) fn view_distance(
        &self,
        information: Option<&crate::protocol::packets::common::ClientInformation>,
    ) -> i32 {
        information.map_or(self.max_view_distance, |info| {
            i32::from(info.view_distance).clamp(world::VIEW_DISTANCE, self.max_view_distance)
        })
    }
}

fn encode_time(time: TimeSnapshot) -> TimeUpdate {
    TimeUpdate {
        world_age: time.world_age,
        time_of_day: time.day_time,
    }
}

fn encode_weather(event: WeatherEvent) -> Clientbound {
    match event {
        WeatherEvent::StartRaining => GameEvent::start_raining().into(),
        WeatherEvent::StopRaining => GameEvent::stop_raining().into(),
        WeatherEvent::RainLevel(level) => GameEvent::rain_level(level).into(),
        WeatherEvent::ThunderLevel(level) => GameEvent::thunder_level(level).into(),
    }
}

fn publish_mutation(players: &crate::players::Players, mutation: WorldMutation) -> io::Result<()> {
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

fn normalize_op(entry: &str) -> String {
    if let Ok(uuid) = uuid::Uuid::parse_str(entry) {
        uuid.to_string()
    } else {
        entry.to_ascii_lowercase()
    }
}
