//! Protocol-agnostic input actions and output state-synchronization events
//! forming the strict boundary between Network IO and the ECS Game Engine.
use crate::{
    config::GameMode,
    game::{Inventory, InventoryClick, Item, ItemStack, Player},
    protocol::packets::play_entities::EntityPose,
    world::{BlockPos, TimeSnapshot, WeatherEvent, WorldMutation},
};
use std::{collections::HashSet, io, sync::mpsc::SyncSender};
use uuid::Uuid;

/// Protocol-agnostic open container state tracked on a player entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerKind {
    Chest(BlockPos),
    CraftingTable {
        grid: [Option<ItemStack>; 9],
        output: Option<ItemStack>,
    },
    Furnace(BlockPos),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenContainer {
    pub window_id: u8,
    pub kind: ContainerKind,
    pub state_id: i32,
}

/// Screen metadata emitted when the ECS opens a container for a player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerOpenSync {
    CraftingTable {
        slots: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
    },
    Chest {
        slots: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
    },
    Furnace {
        slots: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
        properties: [i16; 4],
    },
}

/// Read-only snapshot of an active item drop entity (for joining/respawning players).
#[derive(Debug, Clone, PartialEq)]
pub struct DropSnapshot {
    pub id: i32,
    pub uuid: Uuid,
    pub position: [f64; 3],
    pub velocity: [f64; 3],
    pub stack: ItemStack,
}

/// Read-only snapshot of a player entity for command/query callers.
#[derive(Debug, Clone)]
pub struct PlayerSnapshot {
    pub entity_id: i32,
    pub uuid: Uuid,
    pub name: String,
    pub game_mode: GameMode,
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub pose: EntityPose,
    pub respawn_point: Option<BlockPos>,
}

/// Inbound actions sent from Network IO or server commands into the ECS Game Engine.
#[derive(Debug)]
pub enum GameInput {
    // --- Player lifecycle & protocol-edge view notifications ---
    PlayerJoin {
        player: Box<Player>,
        inventory: Box<Inventory>,
    },
    PlayerLeave {
        uuid: Uuid,
        persist: bool,
    },
    PlayerChunkTracked {
        uuid: Uuid,
        chunk: [i32; 2],
    },
    PlayerChunksUntracked {
        uuid: Uuid,
        chunks: HashSet<[i32; 2]>,
    },
    PlayerTeleportConfirmed {
        uuid: Uuid,
    },

    // --- Client gameplay actions ---
    PlayerMove {
        uuid: Uuid,
        position: Option<[f64; 3]>,
        rotation: Option<[f32; 2]>,
        on_ground: bool,
    },
    PlayerEntityAction {
        uuid: Uuid,
        action: i32,
    },
    PlayerArmAnimation {
        uuid: Uuid,
        hand: i32,
    },
    PlayerRespawn {
        uuid: Uuid,
    },
    PlayerDig {
        uuid: Uuid,
        action: i32,
        pos: BlockPos,
        face: u8,
        sequence: i32,
    },
    PlayerPlace {
        uuid: Uuid,
        pos: BlockPos,
        face: i32,
        hand: i32,
        hit: [f32; 3],
        sequence: i32,
    },
    PlayerUseItem {
        uuid: Uuid,
        hand: i32,
        sequence: i32,
    },
    PlayerSelectSlot {
        uuid: Uuid,
        slot: u8,
    },
    PlayerCreativeSlot {
        uuid: Uuid,
        slot: i16,
        /// `Some(stack_or_none)` if supported by the game registry, `None` if unsupported.
        item: Option<Option<ItemStack>>,
    },
    PlayerInventoryClick {
        uuid: Uuid,
        window: u8,
        state_id: i32,
        click: InventoryClick,
    },
    PlayerCloseWindow {
        uuid: Uuid,
        window_id: u8,
    },

    // --- Server/admin gameplay controls ---
    ControlTeleport {
        uuid: Uuid,
        position: [f64; 3],
        yaw: Option<f32>,
        pitch: Option<f32>,
    },
    ControlGive {
        uuid: Uuid,
        item: Item,
        count: u32,
    },
    ControlSetGameMode {
        uuid: Uuid,
        mode: GameMode,
    },
    ControlSetBlock {
        pos: BlockPos,
        state: i32,
        reply: Option<SyncSender<bool>>,
    },
    ControlFillBlocks {
        from: BlockPos,
        to: BlockPos,
        state: i32,
        reply: Option<SyncSender<io::Result<usize>>>,
    },
    ControlSetDayTime {
        day_time: i64,
        reply: Option<SyncSender<TimeSnapshot>>,
    },
    ControlSetWeather {
        raining: bool,
        thundering: bool,
        duration: u32,
        reply: Option<SyncSender<Vec<WeatherEvent>>>,
    },
}

/// Outbound domain state-synchronization events emitted by the ECS Game Engine
/// and translated into protocol packets by the Network IO layer.
#[derive(Debug)]
pub enum GameOutput {
    // --- World / global broadcasts ---
    TimeUpdated(TimeSnapshot),
    WeatherChanged(WeatherEvent),
    WorldMutated(WorldMutation),
    WorldMutatedWithAck {
        actor: Uuid,
        chunk: [i32; 2],
        mutation: WorldMutation,
        sequence: i32,
    },
    BlockCorrected {
        player: Uuid,
        corrections: Vec<(BlockPos, i32)>,
        sequence: i32,
    },
    BlockAckOnly {
        player: Uuid,
        sequence: i32,
    },
    ChestLidUpdated {
        pos: BlockPos,
        viewer_count: usize,
    },
    BroadcastSystemChat(String),

    // --- Entity broadcasts ---
    ItemDropSpawned {
        id: i32,
        uuid: Uuid,
        position: [f64; 3],
        velocity: [f64; 3],
        stack: ItemStack,
    },
    ItemDropUpdated {
        id: i32,
        stack: ItemStack,
    },
    ItemDropMoved {
        id: i32,
        position: [f64; 3],
        velocity: [f64; 3],
    },
    ItemDropCollected {
        drop_id: i32,
        collector_id: i32,
        count: u8,
    },
    EntitiesRemoved(Vec<i32>),
    BlockDestruction {
        entity_id: i32,
        pos: BlockPos,
        stage: u8,
    },

    // --- Player state & viewer sync ---
    PlayerPoseUpdated {
        uuid: Uuid,
        pose: EntityPose,
    },
    PlayerTeleported {
        uuid: Uuid,
        position: [f64; 3],
        yaw: f32,
        pitch: f32,
    },
    PlayerRespawned {
        uuid: Uuid,
        spawn: [f64; 3],
        game_mode: GameMode,
        health: f32,
        food: i32,
        saturation: f32,
        missing_bed: bool,
        inventory: Inventory,
    },
    PlayerGameModeChanged {
        uuid: Uuid,
        mode: GameMode,
        health: f32,
        food: i32,
        saturation: f32,
        inventory: Inventory,
    },
    PlayerVitalsUpdated {
        uuid: Uuid,
        health: f32,
        food: i32,
        saturation: f32,
    },
    PlayerHurt {
        uuid: Uuid,
        entity_id: i32,
    },
    PlayerMetadataChanged {
        uuid: Uuid,
        entity_id: i32,
        sneaking: bool,
        sprinting: bool,
    },
    PlayerArmSwing {
        uuid: Uuid,
        entity_id: i32,
        hand: i32,
    },
    PlayerEquipmentChanged {
        uuid: Uuid,
        entity_id: i32,
        main_hand: Option<ItemStack>,
        off_hand: Option<ItemStack>,
    },
    PlayerInventorySynced {
        uuid: Uuid,
        inventory: Inventory,
    },
    PlayerSlotSynced {
        uuid: Uuid,
        state_id: i32,
        slot: i16,
        item: Option<ItemStack>,
    },
    PlayerSystemMessage {
        uuid: Uuid,
        message: String,
    },
    ContainerOpened {
        uuid: Uuid,
        window_id: u8,
        screen: ContainerOpenSync,
    },
    ContainerSynced {
        uuid: Uuid,
        window_id: u8,
        state_id: i32,
        slots: Vec<Option<ItemStack>>,
        carried: Option<ItemStack>,
        furnace_props: Option<[i16; 4]>,
    },
    ContainerClosed {
        uuid: Uuid,
        window_id: u8,
    },
    /// Persist a departing player's authoritative state outside the game-loop thread.
    PersistPlayer {
        player: Box<Player>,
        inventory: Box<Inventory>,
    },
}
