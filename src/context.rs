use crate::handler::{Session, login::LoginService, status::StatusService};

/// Shared application services, assembled by the server rather than the dispatcher.
pub struct ServerContext {
    pub status: StatusService,
    pub login: LoginService,
    pub configuration: crate::handler::configuration::ConfigurationService,
    pub play: crate::handler::play::PlayService,
}

impl ServerContext {
    /// Assemble all default services from a validated server configuration.
    pub async fn from_config(config: &crate::config::Config) -> std::io::Result<Self> {
        config.validate()?;
        let mut configuration = crate::handler::configuration::ConfigurationService::new();
        if let Some(pack) = &config.resource_pack {
            configuration = configuration.with_resource_pack(pack.clone())?;
        }
        let world = if let Some(dir) = &config.world_dir {
            let dir = dir.clone();
            let world_type = config.world_type;
            let seed = config.seed;
            let flat = config.flat.clone();
            let opened = tokio::task::spawn_blocking(move || {
                crate::world::World::open_with(&dir, world_type, seed, flat)
            })
            .await
            .map_err(std::io::Error::other)??;
            std::sync::Arc::new(opened)
        } else {
            match config.world_type {
                crate::config::WorldType::Overworld => {
                    let seed = config.seed.unwrap_or_else(rand::random);
                    std::sync::Arc::new(crate::world::World::with_generator(std::sync::Arc::new(
                        crate::world::OverworldGenerator::new(seed),
                    )))
                }
                crate::config::WorldType::Flat => {
                    if let Some(flat) = &config.flat {
                        std::sync::Arc::new(crate::world::World::with_generator(
                            std::sync::Arc::new(crate::world::FlatGenerator::from_config(
                                flat.clone(),
                            )?),
                        ))
                    } else {
                        std::sync::Arc::new(crate::world::World::new())
                    }
                }
            }
        };
        Ok(Self {
            status: StatusService::from_config(config)?,
            login: LoginService::from_config(config).await?,
            configuration,
            play: crate::handler::play::PlayService::with_world(
                config.max_view_distance,
                config.game_mode,
                world,
            )?
            .with_ops(config.ops.clone()),
        })
    }

    /// Advance all server-side gameplay systems by one 20 TPS tick via the ECS engine
    /// (world clock, weather, player vitals/eating, item drops, furnaces, block/fluid physics).
    pub fn tick(&self) {
        if let Err(error) = self.play.tick_engine(self.players()) {
            tracing::warn!(%error, "failed to sync ECS game tick outputs");
        }
    }

    /// Execute a trusted server command without a player connection. Permission
    /// and source are assigned by the caller, never taken from a client packet.
    pub fn execute_command(
        &self,
        source: &crate::command::CommandSource,
        input: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        Ok(self
            .run_command(&crate::command::SourceStack::new(source.clone()), input)
            .result)
    }

    /// Run one bounded Minecraft command chain with a trusted source stack.
    pub fn run_command(
        &self,
        source: &crate::command::SourceStack,
        input: &str,
    ) -> crate::command::ExecutionReport {
        self.play.commands().run(
            input,
            source,
            &mut ServerCommandHost {
                server: self,
                player: self.players().command_host(source),
            },
            crate::command::ExecutionLimits::default(),
        )
    }

    /// Snapshot currently ready entities for command evaluation. The current
    /// world contains players only; returned values do not retain registry locks.
    pub fn command_entities(&self) -> Vec<crate::command::arguments::EntitySnapshot> {
        self.players().command_entities()
    }

    /// Shared admission, presence, and broadcast service.
    pub fn players(&self) -> &crate::players::Players {
        self.status.players()
    }
}

struct ServerCommandHost<'a, H> {
    server: &'a ServerContext,
    player: H,
}

impl<H: crate::command::CommandHost> crate::command::CommandHost for ServerCommandHost<'_, H> {
    fn list_players(&mut self) -> std::io::Result<crate::command::CommandResult> {
        self.player.list_players()
    }
    fn action(
        &mut self,
        player: uuid::Uuid,
        text: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        self.player.action(player, text)
    }
    fn whisper(
        &mut self,
        player: uuid::Uuid,
        target: &str,
        text: &str,
    ) -> std::io::Result<crate::command::CommandResult> {
        self.player.whisper(player, target, text)
    }
    fn command_active(&self, source: &crate::command::SourceStack) -> bool {
        self.player.command_active(source)
    }
    fn command_entities(
        &mut self,
    ) -> std::io::Result<Vec<crate::command::arguments::EntitySnapshot>> {
        Ok(self.server.command_entities())
    }
    fn world(&mut self) -> Option<&mut dyn crate::command::WorldCommandHost> {
        Some(self)
    }
    fn admin(&mut self) -> Option<&mut dyn crate::command::AdminCommandHost> {
        Some(self)
    }
}

impl<H: crate::command::CommandHost> crate::command::WorldCommandHost for ServerCommandHost<'_, H> {
    fn world_time(&mut self) -> std::io::Result<(i64, i64)> {
        Ok(self.server.play.world_time())
    }
    fn set_time(&mut self, day_time: i64) -> std::io::Result<i64> {
        let packet = self.server.play.set_day_time(day_time);
        let _ = self.server.players().broadcast(&packet.into());
        Ok(packet.time_of_day)
    }
    fn set_weather(
        &mut self,
        raining: bool,
        thundering: bool,
        duration: u32,
    ) -> std::io::Result<()> {
        for packet in self.server.play.set_weather(raining, thundering, duration) {
            let _ = self.server.players().broadcast(&packet);
        }
        Ok(())
    }
    fn set_block(&mut self, pos: [i32; 3], state: i32) -> std::io::Result<bool> {
        self.server
            .play
            .set_block(self.server.players(), crate::world::BlockPos(pos), state)
    }
    fn fill_blocks(&mut self, from: [i32; 3], to: [i32; 3], state: i32) -> std::io::Result<usize> {
        self.server.play.fill_blocks(
            self.server.players(),
            crate::world::BlockPos(from),
            crate::world::BlockPos(to),
            state,
        )
    }
}

impl<H: crate::command::CommandHost> crate::command::AdminCommandHost for ServerCommandHost<'_, H> {
    fn teleport_player(
        &mut self,
        uuid: uuid::Uuid,
        position: [f64; 3],
        yaw: Option<f32>,
        pitch: Option<f32>,
    ) -> std::io::Result<bool> {
        if self.server.play.player_snapshot(uuid).is_none() {
            return Ok(false);
        }
        self.server.play.dispatch_input(
            crate::engine::GameInput::ControlTeleport {
                uuid,
                position,
                yaw,
                pitch,
            },
            self.server.players(),
        )?;
        Ok(true)
    }
    fn give_item(
        &mut self,
        uuid: uuid::Uuid,
        item: crate::game::Item,
        count: u32,
    ) -> std::io::Result<bool> {
        if self.server.play.player_snapshot(uuid).is_none() {
            return Ok(false);
        }
        self.server.play.dispatch_input(
            crate::engine::GameInput::ControlGive { uuid, item, count },
            self.server.players(),
        )?;
        Ok(true)
    }
    fn set_game_mode(
        &mut self,
        uuid: uuid::Uuid,
        mode: crate::config::GameMode,
    ) -> std::io::Result<bool> {
        if self.server.play.player_snapshot(uuid).is_none() {
            return Ok(false);
        }
        self.server.play.dispatch_input(
            crate::engine::GameInput::ControlSetGameMode { uuid, mode },
            self.server.players(),
        )?;
        Ok(true)
    }
    fn set_op(&mut self, target: &str, op: bool) -> std::io::Result<crate::command::CommandResult> {
        let online = self.server.players().find_player_by_name(target);
        let key = online
            .as_ref()
            .map(|(_, name)| name.as_str())
            .unwrap_or(target);
        if uuid::Uuid::parse_str(key).is_err() && !crate::auth::valid_username(key) {
            return Ok(crate::command::CommandResult::failure(
                "Invalid player name or UUID.",
            ));
        }
        let changed = if op {
            self.server.play.add_op(key)
        } else {
            self.server.play.remove_op(key)
        };
        if let Some((uuid, _)) = online {
            self.server
                .players()
                .send_control(uuid, crate::players::PlayerControl::RefreshCommands);
        }
        if !changed {
            return Ok(crate::command::CommandResult::failure(if op {
                format!("{key} is already an operator.")
            } else {
                format!("{key} is not an operator.")
            }));
        }
        Ok(crate::command::CommandResult::message(if op {
            format!("Made {key} a server operator.")
        } else {
            format!("Made {key} no longer a server operator.")
        }))
    }
    fn kick_player(&mut self, uuid: uuid::Uuid, reason: &str) -> std::io::Result<bool> {
        Ok(self
            .server
            .players()
            .send_control(uuid, crate::players::PlayerControl::Kick(reason.into())))
    }
    fn request_shutdown(&mut self) -> std::io::Result<bool> {
        Ok(self.server.play.request_shutdown())
    }
    fn save_all(&mut self) -> std::io::Result<String> {
        let summary = self.server.play.save_world()?;
        if !summary.persisted {
            Ok("World persistence is disabled (set world_dir in zcraft.toml).".into())
        } else {
            Ok(format!(
                "Saved the game ({} chunk(s) flushed).",
                summary.chunks_saved
            ))
        }
    }
}

/// Per-dispatch borrows. Handlers have no access to the transport.
pub struct Context<'a> {
    pub session: &'a mut Session,
    pub server: &'a ServerContext,
}
