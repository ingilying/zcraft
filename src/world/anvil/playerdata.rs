//! Per-player `playerdata/<uuid>.dat` GZip NBT persistence and vanilla slot mapping.
use super::{AnvilStorage, DATA_VERSION_1_21_1};
use crate::{
    config::GameMode,
    game::{Inventory, Item, ItemStack, Player, SLOT_COUNT},
    world::{
        BlockPos, SPAWN_X, SPAWN_Y, SPAWN_Z,
        nbt::{Compound, Tag, read_gzip_nbt, write_gzip_nbt_atomic},
    },
};
use std::{io, path::PathBuf};
use uuid::Uuid;

/// Persisted player state restored from `playerdata/<uuid>.dat`.
#[derive(Debug, Clone)]
pub struct SavedPlayer {
    pub position: [f64; 3],
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    pub game_mode: GameMode,
    pub health: f32,
    pub food: i32,
    pub saturation: f32,
    pub respawn_point: Option<BlockPos>,
    pub inventory: Inventory,
}

impl AnvilStorage {
    fn player_path(&self, uuid: Uuid) -> PathBuf {
        self.root.join("playerdata").join(format!("{uuid}.dat"))
    }

    pub fn load_player(&self, uuid: Uuid) -> io::Result<Option<SavedPlayer>> {
        let path = self.player_path(uuid);
        if !path.exists() {
            return Ok(None);
        }
        let (_, root) = read_gzip_nbt(&path)?;
        let pos_list = root.get("Pos").and_then(Tag::as_list);
        let position = match pos_list {
            Some([a, b, c]) => [
                a.as_double().unwrap_or(SPAWN_X),
                b.as_double().unwrap_or(SPAWN_Y),
                c.as_double().unwrap_or(SPAWN_Z),
            ],
            _ => [SPAWN_X, SPAWN_Y, SPAWN_Z],
        };
        let rot_list = root.get("Rotation").and_then(Tag::as_list);
        let (yaw, pitch) = match rot_list {
            Some([y, p]) => (y.as_float().unwrap_or(0.0), p.as_float().unwrap_or(0.0)),
            _ => (0.0, 0.0),
        };
        let on_ground = root.get("OnGround").and_then(Tag::as_byte).unwrap_or(0) != 0;
        let game_mode = match root.get("playerGameType").and_then(Tag::as_int) {
            Some(0) => GameMode::Survival,
            Some(1) => GameMode::Creative,
            _ => GameMode::Spectator,
        };
        let health = root
            .get("Health")
            .and_then(Tag::as_float)
            .unwrap_or(20.0)
            .clamp(0.0, 20.0);
        let food = root
            .get("foodLevel")
            .and_then(Tag::as_int)
            .unwrap_or(20)
            .clamp(0, 20);
        let saturation = root
            .get("foodSaturationLevel")
            .and_then(Tag::as_float)
            .unwrap_or(5.0)
            .clamp(0.0, 20.0);
        let selected = root
            .get("SelectedItemSlot")
            .and_then(Tag::as_int)
            .unwrap_or(0)
            .clamp(0, 8) as u8;

        let mut slots = [None; SLOT_COUNT];
        if let Some(inv) = root.get("Inventory").and_then(Tag::as_list) {
            for item_tag in inv {
                let Some(item_map) = item_tag.as_compound() else {
                    continue;
                };
                let Some(vanilla_slot) = item_map.get("Slot").and_then(Tag::as_byte) else {
                    continue;
                };
                let Some(internal_slot) = vanilla_to_internal_slot(vanilla_slot) else {
                    continue;
                };
                let Some(id_str) = item_map.get("id").and_then(Tag::as_str) else {
                    continue;
                };
                let Some(item) = Item::by_name(id_str) else {
                    continue;
                };
                let count = item_map
                    .get("count")
                    .and_then(Tag::as_int)
                    .or_else(|| item_map.get("Count").and_then(Tag::as_byte).map(i32::from))
                    .unwrap_or(1)
                    .clamp(1, i32::from(item.definition().max_stack))
                    as u8;
                let damage = item_map
                    .get("components")
                    .and_then(Tag::as_compound)
                    .and_then(|c| c.get("minecraft:damage"))
                    .and_then(Tag::as_int)
                    .unwrap_or(0)
                    .max(0) as u32;
                slots[internal_slot] = ItemStack::with_damage(item, count, damage);
            }
        }
        let respawn_point = match (
            root.get("SpawnX").and_then(Tag::as_int),
            root.get("SpawnY").and_then(Tag::as_int),
            root.get("SpawnZ").and_then(Tag::as_int),
        ) {
            (Some(x), Some(y), Some(z)) => Some(BlockPos([x, y, z])),
            _ => None,
        };
        let inventory = Inventory::from_persisted(slots, selected);
        Ok(Some(SavedPlayer {
            position,
            yaw,
            pitch,
            on_ground,
            game_mode,
            health,
            food,
            saturation,
            respawn_point,
            inventory,
        }))
    }

    pub fn save_player(&self, player: &Player, inventory: &Inventory) -> io::Result<()> {
        let mut root = Compound::new();
        root.insert("DataVersion".into(), Tag::Int(DATA_VERSION_1_21_1));
        root.insert(
            "Pos".into(),
            Tag::List(vec![
                Tag::Double(player.position[0]),
                Tag::Double(player.position[1]),
                Tag::Double(player.position[2]),
            ]),
        );
        root.insert(
            "Rotation".into(),
            Tag::List(vec![Tag::Float(player.yaw), Tag::Float(player.pitch)]),
        );
        root.insert("OnGround".into(), Tag::Byte(i8::from(player.on_ground)));
        root.insert(
            "playerGameType".into(),
            Tag::Int(i32::from(player.game_mode.wire_id())),
        );
        root.insert("Health".into(), Tag::Float(player.health));
        root.insert("foodLevel".into(), Tag::Int(player.food));
        root.insert("foodSaturationLevel".into(), Tag::Float(player.saturation));
        if let Some(spawn) = player.respawn_point {
            root.insert("SpawnX".into(), Tag::Int(spawn.0[0]));
            root.insert("SpawnY".into(), Tag::Int(spawn.0[1]));
            root.insert("SpawnZ".into(), Tag::Int(spawn.0[2]));
        }
        root.insert(
            "SelectedItemSlot".into(),
            Tag::Int(i32::from(inventory.selected())),
        );

        let mut inv_list = Vec::new();
        for (idx, stack) in inventory.slots().iter().enumerate() {
            let Some(stack) = stack else {
                continue;
            };
            let Some(vanilla_slot) = internal_to_vanilla_slot(idx) else {
                continue;
            };
            let mut item_map = Compound::new();
            item_map.insert("Slot".into(), Tag::Byte(vanilla_slot));
            item_map.insert("id".into(), Tag::String(stack.item.name().into()));
            item_map.insert("count".into(), Tag::Int(i32::from(stack.count)));
            if stack.damage > 0 {
                let mut components = Compound::new();
                components.insert("minecraft:damage".into(), Tag::Int(stack.damage as i32));
                item_map.insert("components".into(), Tag::Compound(components));
            }
            inv_list.push(Tag::Compound(item_map));
        }
        root.insert("Inventory".into(), Tag::List(inv_list));
        write_gzip_nbt_atomic(&self.player_path(player.uuid), "", &root)
    }
}

fn vanilla_to_internal_slot(slot: i8) -> Option<usize> {
    match slot {
        0..=8 => Some(36 + slot as usize),
        9..=35 => Some(slot as usize),
        100 => Some(8), // Boots
        101 => Some(7), // Leggings
        102 => Some(6), // Chestplate
        103 => Some(5), // Helmet
        -106 => Some(45),
        _ => None,
    }
}

fn internal_to_vanilla_slot(slot: usize) -> Option<i8> {
    match slot {
        36..=44 => Some((slot - 36) as i8),
        9..=35 => Some(slot as i8),
        8 => Some(100), // Boots
        7 => Some(101), // Leggings
        6 => Some(102), // Chestplate
        5 => Some(103), // Helmet
        45 => Some(-106),
        _ => None,
    }
}
