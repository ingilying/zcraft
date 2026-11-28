//! Single-source-of-truth macro defining `GameInput`, typed `bevy_ecs` domain
//! `Messages`, their lifecycle helpers, and the auto-generated `ingress_inputs_system`.
use crate::{
    config::GameMode,
    game::{
        Inventory, InventoryClick, Item, ItemStack, Player, resources::InputQueue,
    },
    world::{BlockPos, TimeSnapshot, WeatherEvent},
};
use bevy_ecs::prelude::{Message, MessageWriter, Messages, ResMut, World};
use std::{collections::HashSet, io, sync::mpsc::SyncSender};
use uuid::Uuid;

macro_rules! define_game_inputs {
    (
        $(
            $(#[$group_meta:meta])*
            group $writer:ident : $msg_ty:ident {
                $(
                    $(#[$var_meta:meta])*
                    $input_var:ident => $msg_var:ident {
                        $(
                            $(#[$field_meta:meta])*
                            $field:ident : $field_ty:ty
                        ),* $(,)?
                    }
                ),* $(,)?
            }
        ),* $(,)?
    ) => {
        /// Inbound actions sent from Network IO or server commands into the ECS Game Engine.
        #[derive(Debug)]
        pub enum GameInput {
            $(
                $(
                    $(#[$var_meta])*
                    $input_var {
                        $(
                            $(#[$field_meta])*
                            $field: $field_ty,
                        )*
                    },
                )*
            )*
        }

        $(
            $(#[$group_meta])*
            #[derive(Message, Debug)]
            pub enum $msg_ty {
                $(
                    $(#[$var_meta])*
                    $msg_var {
                        $(
                            $(#[$field_meta])*
                            $field: $field_ty,
                        )*
                    },
                )*
            }
        )*

        /// Register all input message buffers and internal cross-system messages on `World`.
        pub(crate) fn init_messages(ecs: &mut World) {
            $(
                ecs.init_resource::<Messages<$msg_ty>>();
            )*
            ecs.init_resource::<Messages<SpawnDropMsg>>();
        }

        /// Advance double-buffered message queues so consumed messages are reclaimed.
        pub(crate) fn update_messages(ecs: &mut World) {
            $(
                ecs.resource_mut::<Messages<$msg_ty>>().update();
            )*
            ecs.resource_mut::<Messages<SpawnDropMsg>>().update();
        }

        /// Auto-generated input ingress system: drains `InputQueue` and routes each
        /// `GameInput` into its typed ECS `MessageWriter`.
        #[allow(clippy::too_many_arguments)]
        pub(crate) fn ingress_inputs_system(
            mut queue: ResMut<InputQueue>,
            $( mut $writer: MessageWriter<$msg_ty>, )*
        ) {
            for input in queue.0.drain(..) {
                match input {
                    $(
                        $(
                            GameInput::$input_var { $( $field, )* } => {
                                $writer.write($msg_ty::$msg_var { $( $field, )* });
                            }
                        )*
                    )*
                }
            }
        }
    };
}

define_game_inputs! {
    group joins : PlayerJoinMsg {
        PlayerJoin => Join {
            player: Box<Player>,
            inventory: Box<Inventory>,
        },
    },
    group leaves : PlayerLeaveMsg {
        PlayerLeave => Leave {
            uuid: Uuid,
            persist: bool,
        },
    },
    group views : PlayerViewMsg {
        PlayerChunkTracked => ChunkTracked {
            uuid: Uuid,
            chunk: [i32; 2],
        },
        PlayerChunksUntracked => ChunksUntracked {
            uuid: Uuid,
            chunks: HashSet<[i32; 2]>,
        },
        PlayerTeleportConfirmed => TeleportConfirmed {
            uuid: Uuid,
        },
    },
    group player_controls : PlayerControlMsg {
        ControlTeleport => Teleport {
            uuid: Uuid,
            position: [f64; 3],
            yaw: Option<f32>,
            pitch: Option<f32>,
        },
        ControlGive => Give {
            uuid: Uuid,
            item: Item,
            count: u32,
        },
        ControlSetGameMode => SetGameMode {
            uuid: Uuid,
            mode: GameMode,
        },
    },
    group world_controls : WorldControlMsg {
        ControlSetBlock => SetBlock {
            pos: BlockPos,
            state: i32,
            reply: Option<SyncSender<bool>>,
        },
        ControlFillBlocks => FillBlocks {
            from: BlockPos,
            to: BlockPos,
            state: i32,
            reply: Option<SyncSender<io::Result<usize>>>,
        },
        ControlSetDayTime => SetDayTime {
            day_time: i64,
            reply: Option<SyncSender<TimeSnapshot>>,
        },
        ControlSetWeather => SetWeather {
            raining: bool,
            thundering: bool,
            duration: u32,
            reply: Option<SyncSender<Vec<WeatherEvent>>>,
        },
    },
    group movements : MovementMsg {
        PlayerMove => Move {
            uuid: Uuid,
            position: Option<[f64; 3]>,
            rotation: Option<[f32; 2]>,
            on_ground: bool,
        },
        PlayerEntityAction => EntityAction {
            uuid: Uuid,
            action: i32,
        },
        PlayerArmAnimation => ArmAnimation {
            uuid: Uuid,
            hand: i32,
        },
        PlayerRespawn => Respawn {
            uuid: Uuid,
        },
    },
    group interactions : InteractionMsg {
        PlayerDig => Dig {
            uuid: Uuid,
            action: i32,
            pos: BlockPos,
            face: u8,
            sequence: i32,
        },
        PlayerPlace => Place {
            uuid: Uuid,
            pos: BlockPos,
            face: i32,
            hand: i32,
            hit: [f32; 3],
            sequence: i32,
        },
        PlayerUseItem => UseItem {
            uuid: Uuid,
            hand: i32,
            sequence: i32,
        },
        PlayerSelectSlot => SelectSlot {
            uuid: Uuid,
            slot: u8,
        },
        PlayerCreativeSlot => CreativeSlot {
            uuid: Uuid,
            slot: i16,
            /// `Some(stack_or_none)` if supported by the game registry, `None` if unsupported.
            item: Option<Option<ItemStack>>,
        },
        PlayerInventoryClick => InventoryClick {
            uuid: Uuid,
            window: u8,
            state_id: i32,
            click: InventoryClick,
        },
        PlayerCloseWindow => CloseWindow {
            uuid: Uuid,
            window_id: u8,
        },
    },
}

/// Request to spawn a dropped item entity in the ECS world.
/// If `velocity` is `None`, vanilla random horizontal scatter is applied.
#[derive(Message, Debug, Clone, Copy)]
pub struct SpawnDropMsg {
    pub position: [f64; 3],
    pub velocity: Option<[f64; 3]>,
    pub stack: ItemStack,
    pub pickup_delay: u16,
}

impl SpawnDropMsg {
    pub fn scatter(position: [f64; 3], stack: ItemStack, pickup_delay: u16) -> Self {
        Self {
            position,
            velocity: None,
            stack,
            pickup_delay,
        }
    }

    pub fn with_velocity(
        position: [f64; 3],
        velocity: [f64; 3],
        stack: ItemStack,
        pickup_delay: u16,
    ) -> Self {
        Self {
            position,
            velocity: Some(velocity),
            stack,
            pickup_delay,
        }
    }
}
