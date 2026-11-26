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
                CollectItem, EntityMetadata, EntityPose, EntityTeleport, EntityVelocity,
                RemoveEntities, SpawnEntity,
            },
        },
    },
    world::{self, BlockPos, TimeSnapshot, WeatherEvent, World, WorldMutation},
};
use rand::Rng;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
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
    uuid: uuid::Uuid,
    position: [f64; 3],
    last_synced_pos: [f64; 3],
    velocity: [f64; 3],
    stack: crate::game::ItemStack,
    pickup_delay: u16,
    age: u16,
    ticks_since_sync: u16,
}

pub struct PlayService {
    world: Arc<World>,
    drops: Mutex<Vec<ItemDrop>>,
    chest_viewers: Mutex<HashMap<BlockPos, HashSet<uuid::Uuid>>>,
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
        Self::with_options(max_view_distance, GameMode::default())
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
            chest_viewers: Mutex::new(HashMap::new()),
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

    /// Track a player opening a chest and return the updated total viewer count.
    pub fn open_chest_viewer(&self, pos: BlockPos, uuid: uuid::Uuid) -> usize {
        let mut viewers = self.chest_viewers.lock().expect("chest viewers poisoned");
        let entry = viewers.entry(pos).or_default();
        entry.insert(uuid);
        entry.len()
    }

    /// Track a player closing a chest and return the updated total viewer count.
    pub fn close_chest_viewer(&self, pos: BlockPos, uuid: uuid::Uuid) -> usize {
        let mut viewers = self.chest_viewers.lock().expect("chest viewers poisoned");
        if let Some(entry) = viewers.get_mut(&pos) {
            entry.remove(&uuid);
            let count = entry.len();
            if count == 0 {
                viewers.remove(&pos);
            }
            count
        } else {
            0
        }
    }

    /// Remove a disconnecting player from all open chests, returning `(pos, remaining_count)`.
    pub fn remove_player_from_chests(&self, uuid: uuid::Uuid) -> Vec<(BlockPos, usize)> {
        let mut viewers = self.chest_viewers.lock().expect("chest viewers poisoned");
        let mut changed = Vec::new();
        viewers.retain(|&pos, set| {
            if set.remove(&uuid) {
                changed.push((pos, set.len()));
            }
            !set.is_empty()
        });
        changed
    }

    /// Return UUIDs of other players currently viewing the chest at `pos`.
    pub fn other_chest_viewers(&self, pos: BlockPos, exclude: uuid::Uuid) -> Vec<uuid::Uuid> {
        let viewers = self.chest_viewers.lock().expect("chest viewers poisoned");
        viewers
            .get(&pos)
            .map(|set| set.iter().copied().filter(|&id| id != exclude).collect())
            .unwrap_or_default()
    }

    /// Spawn a dropped `ItemEntity` with vanilla `ItemEntity.<init>` random horizontal scatter.
    pub fn spawn_item_drop_scatter(
        &self,
        players: &crate::players::Players,
        position: [f64; 3],
        stack: crate::game::ItemStack,
        pickup_delay: u16,
    ) -> io::Result<()> {
        let mut rng = rand::thread_rng();
        let vx = rng.r#gen::<f64>() * 0.2 - 0.1;
        let vz = rng.r#gen::<f64>() * 0.2 - 0.1;
        self.spawn_item_drop(players, position, [vx, 0.2, vz], stack, pickup_delay)
    }

    /// Build spawn/metadata/velocity packets for all active `ItemDrop`s (for joining/respawning players).
    pub fn existing_drop_packets(&self) -> Vec<Clientbound> {
        let drops = self.drops.lock().expect("item drops poisoned");
        let mut packets = Vec::with_capacity(drops.len() * 3);
        for drop in drops.iter() {
            packets
                .push(SpawnEntity::item(drop.id, drop.uuid, drop.position, drop.velocity).into());
            packets.push(
                EntityMetadata::Item {
                    id: drop.id,
                    stack: drop.stack,
                }
                .into(),
            );
            packets.push(EntityVelocity::new(drop.id, drop.velocity).into());
        }
        packets
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
                uuid,
                position,
                last_synced_pos: position,
                velocity,
                stack,
                pickup_delay,
                age: 0,
                ticks_since_sync: 0,
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

    /// Atomically attempt to pick up `drop_id` into `inventory`, preserving `damage`
    /// and leaving uncollected items on the ground if the inventory is full.
    pub fn try_pickup_drop(
        &self,
        players: &crate::players::Players,
        drop_id: i32,
        player_id: i32,
        inventory: &mut crate::game::Inventory,
    ) -> bool {
        let collect_count;
        let mut remaining_stack: Option<crate::game::ItemStack> = None;
        let mut removed = false;
        {
            let mut drops = self.drops.lock().expect("item drops poisoned");
            if let Some(idx) = drops
                .iter()
                .position(|d| d.id == drop_id && d.pickup_delay == 0)
            {
                let added = inventory.give_stack(drops[idx].stack);
                if added == 0 {
                    return false;
                }
                collect_count = added;
                if added >= drops[idx].stack.count {
                    drops.remove(idx);
                    removed = true;
                } else {
                    drops[idx].stack.count -= added;
                    remaining_stack = Some(drops[idx].stack);
                }
            } else {
                return false;
            }
        }
        let _ = players.broadcast_entity_packet(
            &CollectItem {
                collected_id: drop_id,
                collector_id: player_id,
                count: i32::from(collect_count),
            }
            .into(),
        );
        if removed {
            let _ = players.broadcast_entity_packet(&RemoveEntities(vec![drop_id]).into());
        } else if let Some(stack) = remaining_stack {
            let _ = players
                .broadcast_entity_packet(&EntityMetadata::Item { id: drop_id, stack }.into());
        }
        true
    }

    /// Advance dropped item physics, merging, periodic sync, expiration, and
    /// proximity pickup requests (20 TPS), aligned with vanilla `ItemEntity.tick`.
    pub fn tick_drops(&self, players: &crate::players::Players) {
        let candidates = players.pickup_candidates();
        let mut pickup_requests: Vec<(uuid::Uuid, i32)> = Vec::new();
        let mut expired: Vec<i32> = Vec::new();
        let mut merged_removed: Vec<i32> = Vec::new();
        let mut merged_updated: Vec<(i32, crate::game::ItemStack)> = Vec::new();
        let mut pos_syncs: Vec<(i32, [f64; 3], [f64; 3])> = Vec::new();
        {
            let mut drops = self.drops.lock().expect("item drops poisoned");
            drops.retain_mut(|drop| {
                drop.age = drop.age.saturating_add(1);
                if drop.age >= 6000 || drop.position[1] < f64::from(world::MIN_Y) - 64.0 {
                    expired.push(drop.id);
                    return false;
                }
                drop.pickup_delay = drop.pickup_delay.saturating_sub(1);

                // Vanilla ItemEntity gravity (0.04/tick) and drag (0.98 air, 0.6*0.98 ground).
                drop.velocity[1] = (drop.velocity[1] - 0.04).max(-1.0);

                // Horizontal move with basic solid block collision.
                for axis in [0usize, 2usize] {
                    let next = drop.position[axis] + drop.velocity[axis];
                    let mut test = drop.position;
                    test[axis] = next;
                    let bp = BlockPos([
                        test[0].floor() as i32,
                        test[1].floor() as i32,
                        test[2].floor() as i32,
                    ]);
                    if bp.valid()
                        && crate::game::block_by_state(self.world.block_at(bp))
                            .is_some_and(|d| d.collision)
                    {
                        drop.velocity[axis] = 0.0;
                    } else {
                        drop.position[axis] = next;
                    }
                }
                drop.position[1] += drop.velocity[1];

                let below = BlockPos([
                    drop.position[0].floor() as i32,
                    (drop.position[1] - 0.125).floor() as i32,
                    drop.position[2].floor() as i32,
                ]);
                let on_ground = below.valid()
                    && crate::game::block_by_state(self.world.block_at(below))
                        .is_some_and(|d| d.collision);
                if on_ground {
                    drop.position[1] = f64::from(below.0[1] + 1) + 0.125;
                    let ground_drag = 0.6 * 0.98;
                    drop.velocity[0] *= ground_drag;
                    drop.velocity[2] *= ground_drag;
                    if drop.velocity[1] < 0.0 {
                        drop.velocity[1] *= -0.5;
                        if drop.velocity[1].abs() < 0.05 {
                            drop.velocity[1] = 0.0;
                        }
                    }
                } else {
                    drop.velocity[0] *= 0.98;
                    drop.velocity[1] *= 0.98;
                    drop.velocity[2] *= 0.98;
                }

                // Periodic position/velocity sync when moving or after significant drift.
                drop.ticks_since_sync = drop.ticks_since_sync.saturating_add(1);
                let dx = drop.position[0] - drop.last_synced_pos[0];
                let dy = drop.position[1] - drop.last_synced_pos[1];
                let dz = drop.position[2] - drop.last_synced_pos[2];
                let moved_sq = dx * dx + dy * dy + dz * dz;
                if moved_sq > 0.04 || (drop.ticks_since_sync >= 20 && moved_sq > 1e-4) {
                    drop.last_synced_pos = drop.position;
                    drop.ticks_since_sync = 0;
                    pos_syncs.push((drop.id, drop.position, drop.velocity));
                }

                if drop.pickup_delay == 0 {
                    for &(uuid, _player_id, ppos) in &candidates {
                        let dx = ppos[0] - drop.position[0];
                        let dy = (ppos[1] + 0.9) - drop.position[1];
                        let dz = ppos[2] - drop.position[2];
                        if dx * dx + dy * dy + dz * dz <= 2.25 {
                            pickup_requests.push((uuid, drop.id));
                            break;
                        }
                    }
                }
                true
            });

            // Merge nearby identical stacks (matching vanilla `ItemEntity.mergeWithNeighbours`).
            let mut i = 0;
            while i < drops.len() {
                let moving = drops[i].velocity[0].abs() + drops[i].velocity[2].abs() > 0.003;
                let interval = if moving { 2 } else { 40 };
                if !drops[i].age.is_multiple_of(interval) {
                    i += 1;
                    continue;
                }
                let max_stack = drops[i].stack.definition().max_stack;
                if drops[i].stack.count >= max_stack {
                    i += 1;
                    continue;
                }
                let mut j = i + 1;
                while j < drops.len() {
                    let same = drops[i].stack.item == drops[j].stack.item
                        && drops[i].stack.damage == drops[j].stack.damage;
                    if same && drops[i].stack.count < max_stack {
                        let dx = drops[i].position[0] - drops[j].position[0];
                        let dy = drops[i].position[1] - drops[j].position[1];
                        let dz = drops[i].position[2] - drops[j].position[2];
                        if dx * dx + dy * dy + dz * dz <= 0.25 {
                            let space = max_stack - drops[i].stack.count;
                            let take = space.min(drops[j].stack.count);
                            drops[i].stack.count += take;
                            drops[i].age = drops[i].age.min(drops[j].age);
                            drops[i].pickup_delay =
                                drops[i].pickup_delay.max(drops[j].pickup_delay);
                            merged_updated.push((drops[i].id, drops[i].stack));
                            if take == drops[j].stack.count {
                                merged_removed.push(drops[j].id);
                                drops.remove(j);
                                continue;
                            } else {
                                drops[j].stack.count -= take;
                                merged_updated.push((drops[j].id, drops[j].stack));
                            }
                        }
                    }
                    j += 1;
                }
                i += 1;
            }
        }
        expired.extend(merged_removed);
        if !expired.is_empty() {
            let _ = players.broadcast_entity_packet(&RemoveEntities(expired).into());
        }
        for (id, stack) in merged_updated {
            let _ = players.broadcast_entity_packet(&EntityMetadata::Item { id, stack }.into());
        }
        for (id, pos, vel) in pos_syncs {
            let _ = players.broadcast_entity_packet(
                &EntityTeleport {
                    id,
                    pose: EntityPose::new(pos, 0.0, 0.0, false),
                }
                .into(),
            );
            let _ = players.broadcast_entity_packet(&EntityVelocity::new(id, vel).into());
        }
        for (uuid, drop_id) in pickup_requests {
            players.send_control(uuid, crate::players::PlayerControl::TryPickup(drop_id));
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
