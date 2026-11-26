//! Event loop, connection state machine, and gameplay.
//!
//! One thread drives everything: a `mio` poll loop dispatches socket events,
//! and a fixed 20 Hz tick streams chunks, batches entity movement, and runs
//! timers. Outbound packets are encoded once and shared between recipients.

use crate::chunk::{write_level_chunk, LEVEL_CHUNK_SIZE_HINT};
use crate::inventory::{Inventory, Slot, INVENTORY_SIZE};
use crate::items::{block_state_from_item, for_each_drop, AIR_ITEM};
use crate::metrics::{RuntimeMetrics, RuntimeSnapshot};
use crate::protocol::{
    inflate_payload, next_frame, read_var_i32_from_slice, valid_name, Cursor, Frame, PacketWriter,
    CLIENT_VERSION, COMPRESSION_THRESHOLD, PROTOCOL_VERSION,
};
use crate::recipes::crafted_item;
use crate::save::{self, SaveData};
use crate::vanilla::{self, block, packet};
use crate::world::{World, MIN_Y, SECTION_COUNT, VIEW_DISTANCE};
use crate::Result;
use mio::net::{TcpListener, TcpStream};
use mio::{Events, Interest, Poll, Token};
use std::collections::{HashMap, VecDeque};
use std::fmt::Arguments;
use std::io::{self, BufWriter, IoSlice, Read, Stdout, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

const LISTENER: Token = Token(0);
const TICK: Duration = Duration::from_millis(50);
const CHUNKS_PER_TICK: usize = 10;
const CHUNK_CACHE_CAPACITY: usize = 48;
const WRITE_BATCH: usize = 64;
/// A client that stops reading is dropped once this much output is queued.
const MAX_QUEUED_BYTES: usize = 1 << 20;
/// A connection that is closing gets this long to drain its farewell packet.
const CLOSE_GRACE_TICKS: u64 = 5 * 20;
/// Largest movement accepted in one packet before the player is pulled back.
const MAX_MOVE_PER_PACKET: f64 = 100.0;
const KEEPALIVE_TICKS: u64 = 15 * 20;
const LOGIN_TIMEOUT_TICKS: u64 = 30 * 20;
const TIME_SYNC_TICKS: u64 = 10 * 20;
const SAVE_TICKS: u64 = 10 * 20;
const CHAT_BURST: u8 = 10;
const CHAT_REFILL_TICKS: u64 = 5;
const MAX_CHAT_CHARS: usize = 256;
/// Squared interaction reach measured from the eyes, as vanilla allows.
const REACH_SQ: f64 = 8.0 * 8.0;
const EYE_HEIGHT: f64 = 1.62;
const SEA_LEVEL: i32 = 63;

pub struct Config {
    pub bind: SocketAddr,
    pub max_players: usize,
    pub compression: bool,
    pub seed: u64,
    pub world_path: Option<PathBuf>,
    pub motd: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Handshake,
    Status,
    Login,
    Configuration,
    Play,
}

/// A queued slice of a shared outbound buffer.
struct Pending {
    data: Rc<Frame>,
    start: u32,
    end: u32,
}

struct Conn {
    socket: TcpStream,
    phase: Phase,
    rx: Vec<u8>,
    tx: VecDeque<Pending>,
    tx_bytes: usize,
    compression: bool,
    close_after_flush: bool,
    closing_since: u64,
    kick: Option<&'static str>,
    connected_tick: u64,
    name: String,
    player: Option<Box<Player>>,
}

struct Player {
    entity_id: i32,
    uuid: u128,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
    on_ground: bool,
    moved: bool,
    rotated: bool,
    center: (i32, i32),
    /// Chunks still to send, sorted so the nearest is at the end.
    chunk_queue: Vec<(i32, i32)>,
    keepalive_pending: bool,
    chat_tokens: u8,
    skin_parts: u8,
    sneaking: bool,
    hotbar: usize,
    inventory: Inventory,
    crafting: [Slot; 9],
}

struct ChunkCache {
    entries: HashMap<(i32, i32), (Rc<Frame>, u64)>,
    clock: u64,
}

impl ChunkCache {
    fn get(&mut self, key: (i32, i32)) -> Option<Rc<Frame>> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(&key).map(|(frame, used)| {
            *used = clock;
            frame.clone()
        })
    }

    fn insert(&mut self, key: (i32, i32), frame: Rc<Frame>) {
        if self.entries.len() >= CHUNK_CACHE_CAPACITY {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (frame, self.clock));
    }

    fn invalidate(&mut self, key: (i32, i32)) {
        self.entries.remove(&key);
    }
}

pub struct Server {
    poll: Poll,
    listener: TcpListener,
    conns: Vec<Option<Conn>>,
    free: Vec<usize>,
    open_conns: usize,
    online: usize,
    next_entity_id: i32,
    next_tick: Instant,
    tick: u64,
    day_time: i64,
    rng: u64,
    config: Config,
    world: World,
    world_dirty: bool,
    inventories: HashMap<String, Inventory>,
    chunks: ChunkCache,
    configuration_burst: Rc<Frame>,
    inflate_scratch: Vec<u8>,
    touched: Vec<usize>,
    log: BufWriter<Stdout>,
    metrics: RuntimeMetrics,
    metrics_active: bool,
    metrics_reported: bool,
}

impl Server {
    pub fn bind(config: Config) -> Result<Self> {
        let mut log = BufWriter::with_capacity(8 * 1024, io::stdout());
        let mut seed = config.seed;
        let mut saved_edits = Vec::new();
        let mut inventories = HashMap::new();
        if let Some(path) = &config.world_path {
            if let Some(data) = save::load(path)? {
                if data.seed != seed {
                    log_line(
                        &mut log,
                        "INFO",
                        format_args!("using seed {} stored in {}", data.seed, path.display()),
                    );
                }
                seed = data.seed;
                saved_edits = data.edits;
                inventories = data.inventories.into_iter().collect();
            }
        }
        let mut world = World::new(seed);
        for (x, y, z, state) in saved_edits {
            world.set_block(x, y, z, state);
        }

        let poll = Poll::new()?;
        let mut listener = TcpListener::bind(config.bind)?;
        poll.registry()
            .register(&mut listener, LISTENER, Interest::READABLE)?;
        log_line(
            &mut log,
            "INFO",
            format_args!(
                "sculk {} listening on {} (Minecraft {CLIENT_VERSION}, protocol {PROTOCOL_VERSION}, seed {seed})",
                env!("CARGO_PKG_VERSION"),
                config.bind
            ),
        );
        log.flush()?;
        let configuration_burst = Rc::new(configuration_burst(config.compression));
        Ok(Self {
            poll,
            listener,
            conns: Vec::new(),
            free: Vec::new(),
            open_conns: 0,
            online: 0,
            next_entity_id: 1,
            next_tick: Instant::now() + TICK,
            tick: 0,
            day_time: 1000,
            rng: (seed ^ 0x6a09_e667_f3bc_c909) | 1,
            config,
            world,
            world_dirty: false,
            inventories,
            chunks: ChunkCache {
                entries: HashMap::new(),
                clock: 0,
            },
            configuration_burst,
            inflate_scratch: Vec::new(),
            touched: Vec::new(),
            log,
            metrics: RuntimeMetrics::default(),
            metrics_active: false,
            metrics_reported: false,
        })
    }

    pub fn run(&mut self) -> Result<()> {
        let mut events = Events::with_capacity(256);
        loop {
            let timeout = self.next_tick.saturating_duration_since(Instant::now());
            if let Err(err) = self.poll.poll(&mut events, Some(timeout)) {
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.into());
            }
            for event in &events {
                if event.token() == LISTENER {
                    self.accept();
                    continue;
                }
                let idx = event.token().0 - 1;
                if event.is_readable() || event.is_read_closed() || event.is_error() {
                    self.on_readable(idx);
                }
                if event.is_writable() {
                    self.flush(idx);
                }
            }
            while let Some(idx) = self.touched.pop() {
                self.flush(idx);
            }

            let now = Instant::now();
            if now >= self.next_tick {
                self.tick();
                let elapsed = now.elapsed();
                if self.metrics_active {
                    self.metrics.tick.record(elapsed);
                }
                self.next_tick += TICK;
                if self.next_tick < now {
                    self.next_tick = now + TICK;
                }
            }
        }
    }

    // ---------------------------------------------------------------- sockets

    fn accept(&mut self) {
        loop {
            let (socket, addr) = match self.listener.accept() {
                Ok(accepted) => accepted,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    self.warn(format_args!("accept failed: {err}"));
                    return;
                }
            };
            if self.open_conns >= self.config.max_players + 64 {
                continue; // dropping the socket closes it
            }
            let mut socket = socket;
            let _ = socket.set_nodelay(true);
            limit_kernel_buffers(&socket);
            let idx = self.free.pop().unwrap_or(self.conns.len());
            if let Err(err) = self.poll.registry().register(
                &mut socket,
                Token(idx + 1),
                Interest::READABLE | Interest::WRITABLE,
            ) {
                self.warn(format_args!("could not register {addr}: {err}"));
                self.free.push(idx);
                continue;
            }
            let conn = Conn {
                socket,
                phase: Phase::Handshake,
                rx: Vec::new(),
                tx: VecDeque::new(),
                tx_bytes: 0,
                compression: false,
                close_after_flush: false,
                closing_since: 0,
                kick: None,
                connected_tick: self.tick,
                name: String::new(),
                player: None,
            };
            if idx == self.conns.len() {
                self.conns.push(Some(conn));
            } else {
                self.conns[idx] = Some(conn);
            }
            self.open_conns += 1;
        }
    }

    fn on_readable(&mut self, idx: usize) {
        let mut buf = [0u8; 4096];
        loop {
            let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) else {
                return;
            };
            let read = match conn.socket.read(&mut buf) {
                Ok(0) => {
                    self.remove(idx);
                    return;
                }
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.remove(idx);
                    return;
                }
            };
            let mut pending = std::mem::take(&mut conn.rx);
            let outcome = if pending.is_empty() {
                self.process_frames(idx, &buf[..read])
                    .map(|used| (used, false))
            } else {
                pending.extend_from_slice(&buf[..read]);
                self.process_frames(idx, &pending).map(|used| (used, true))
            };
            match outcome {
                Ok((used, buffered)) => {
                    let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) else {
                        return;
                    };
                    if buffered {
                        pending.drain(..used);
                        conn.rx = pending;
                    } else if used < read {
                        conn.rx.extend_from_slice(&buf[used..read]);
                    }
                    if conn.rx.is_empty() && conn.rx.capacity() > 16 * 1024 {
                        conn.rx = Vec::new();
                    }
                }
                Err(err) => {
                    let name = self.conn_label(idx);
                    self.info(format_args!("dropping {name}: {err}"));
                    self.kick(idx, "Protocol error");
                    return;
                }
            }
            self.touched.push(idx);
            if read < buf.len() {
                return;
            }
        }
    }

    /// Handles every complete frame in `data`; returns the bytes consumed.
    fn process_frames(&mut self, idx: usize, data: &[u8]) -> Result<usize> {
        let mut pos = 0;
        loop {
            let open = self
                .conns
                .get(idx)
                .and_then(Option::as_ref)
                .is_some_and(|c| !c.close_after_flush && c.kick.is_none());
            if !open {
                return Ok(data.len());
            }
            let Some((start, end)) = next_frame(data, &mut pos)? else {
                return Ok(pos);
            };
            self.handle_frame(idx, &data[start..end])?;
        }
    }

    fn handle_frame(&mut self, idx: usize, frame: &[u8]) -> Result<()> {
        let compression = self.conns[idx].as_ref().is_some_and(|c| c.compression);
        if !compression {
            return self.handle_packet(idx, frame);
        }
        let mut pos = 0;
        let data_len = read_var_i32_from_slice(frame, &mut pos)?;
        if data_len == 0 {
            return self.handle_packet(idx, &frame[pos..]);
        }
        if data_len < 0 {
            return Err(invalid("negative data length"));
        }
        let mut scratch = std::mem::take(&mut self.inflate_scratch);
        let result = inflate_payload(&frame[pos..], data_len as usize, &mut scratch)
            .and_then(|()| self.handle_packet(idx, &scratch));
        if scratch.capacity() <= 64 * 1024 {
            self.inflate_scratch = scratch;
        }
        result
    }

    fn flush(&mut self, idx: usize) {
        let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) else {
            return;
        };
        while !conn.tx.is_empty() {
            let mut slices = [IoSlice::new(&[]); WRITE_BATCH];
            let mut count = 0;
            for (slot, pending) in slices.iter_mut().zip(conn.tx.iter()) {
                *slot = IoSlice::new(&pending.data[pending.start as usize..pending.end as usize]);
                count += 1;
            }
            match conn.socket.write_vectored(&slices[..count]) {
                Ok(0) => break,
                Ok(mut written) => {
                    conn.tx_bytes -= written;
                    while written > 0 {
                        let front = conn.tx.front_mut().expect("written bytes were queued");
                        let len = (front.end - front.start) as usize;
                        if written < len {
                            front.start += written as u32;
                            break;
                        }
                        written -= len;
                        conn.tx.pop_front();
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.remove(idx);
                    return;
                }
            }
        }
        if conn.tx.capacity() > 4 * WRITE_BATCH {
            conn.tx.shrink_to(WRITE_BATCH);
        }
        if conn.close_after_flush {
            self.remove(idx);
        }
    }

    fn send(&mut self, idx: usize, frame: Frame) {
        self.send_shared(idx, &Rc::new(frame));
    }

    fn send_shared(&mut self, idx: usize, frame: &Rc<Frame>) {
        let len = frame.len() as u32;
        self.send_slice(idx, frame, 0, len);
    }

    fn send_slice(&mut self, idx: usize, frame: &Rc<Frame>, start: u32, end: u32) {
        if start >= end {
            return;
        }
        let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) else {
            return;
        };
        if conn.close_after_flush || conn.kick.is_some() {
            return;
        }
        conn.tx_bytes += (end - start) as usize;
        if conn.tx_bytes > MAX_QUEUED_BYTES {
            // The client is not reading; drop everything and close at the next tick.
            conn.tx.clear();
            conn.tx_bytes = 0;
            conn.kick = Some("");
            return;
        }
        conn.tx.push_back(Pending {
            data: frame.clone(),
            start,
            end,
        });
    }

    /// Sends a packet to every player in play, optionally skipping one.
    fn broadcast(&mut self, frame: Frame, except: Option<usize>) {
        let frame = Rc::new(frame);
        for idx in 0..self.conns.len() {
            if Some(idx) != except && self.in_play(idx) {
                self.send_shared(idx, &frame);
            }
        }
    }

    /// Sends a farewell message (when the phase allows one), flushes, and closes.
    fn kick(&mut self, idx: usize, reason: &str) {
        let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) else {
            return;
        };
        if reason.is_empty() {
            self.remove(idx);
            return;
        }
        conn.kick = None;
        let frame = match conn.phase {
            Phase::Login => Some(login_disconnect(reason)),
            Phase::Configuration | Phase::Play => {
                let id = if conn.phase == Phase::Play {
                    packet::play::cb::DISCONNECT
                } else {
                    packet::configuration::cb::DISCONNECT
                };
                let mut p = PacketWriter::new(id);
                p.nbt_text(reason);
                Some(p.finish(conn.compression))
            }
            _ => None,
        };
        if let Some(frame) = frame {
            self.send(idx, frame);
        }
        let tick = self.tick;
        if let Some(conn) = self.conns.get_mut(idx).and_then(Option::as_mut) {
            conn.close_after_flush = true;
            conn.closing_since = tick;
        }
        self.flush(idx);
    }

    fn remove(&mut self, idx: usize) {
        let Some(mut conn) = self.conns.get_mut(idx).and_then(Option::take) else {
            return;
        };
        let _ = self.poll.registry().deregister(&mut conn.socket);
        self.free.push(idx);
        self.open_conns -= 1;
        let Some(player) = conn.player.take() else {
            return;
        };
        self.inventories
            .insert(conn.name.clone(), player.inventory.clone());
        self.world_dirty = true;
        if conn.phase != Phase::Play {
            return;
        }
        self.online -= 1;
        if self.metrics_active && !self.metrics_reported {
            let snapshot = self.metrics.snapshot();
            log_runtime_metrics(&mut self.log, snapshot);
            self.metrics_reported = true;
        }
        if self.online == 0 {
            self.metrics_active = false;
        }
        self.info(format_args!("{} left", conn.name));
        let compression = self.config.compression;
        self.broadcast(
            system_chat(&format!("§e{} left the game", conn.name), compression),
            None,
        );
        let mut p = PacketWriter::new(packet::play::cb::PLAYER_INFO_REMOVE);
        p.var_i32(1);
        p.uuid(player.uuid);
        self.broadcast(p.finish(compression), None);
        let mut p = PacketWriter::new(packet::play::cb::REMOVE_ENTITIES);
        p.var_i32(1);
        p.var_i32(player.entity_id);
        self.broadcast(p.finish(compression), None);
    }

    // ---------------------------------------------------------------- packets

    fn handle_packet(&mut self, idx: usize, payload: &[u8]) -> Result<()> {
        let mut c = Cursor::new(payload);
        let id = c.var_i32()?;
        let phase = self.conns[idx]
            .as_ref()
            .map_or(Phase::Handshake, |c| c.phase);
        match (phase, id) {
            (Phase::Handshake, packet::handshake::sb::INTENTION) => {
                let protocol = c.var_i32()?;
                let _host = c.str(255)?;
                let _port = c.i16()?;
                let next = c.var_i32()?;
                let conn = self.conns[idx].as_mut().expect("connection exists");
                match next {
                    1 => conn.phase = Phase::Status,
                    2 | 3 => {
                        conn.phase = Phase::Login;
                        if protocol != PROTOCOL_VERSION {
                            let reason = format!("This server runs Minecraft {CLIENT_VERSION}");
                            self.kick(idx, &reason);
                        }
                    }
                    _ => return Err(invalid("unknown handshake intent")),
                }
            }
            (Phase::Status, packet::status::sb::STATUS_REQUEST) => {
                let frame =
                    status_response(&self.config.motd, self.config.max_players, self.online);
                self.send(idx, frame);
            }
            (Phase::Status, packet::status::sb::PING_REQUEST) => {
                let mut p = PacketWriter::new(packet::status::cb::PONG_RESPONSE);
                p.i64(c.i64()?);
                self.send(idx, p.finish(false));
                let tick = self.tick;
                if let Some(conn) = self.conns[idx].as_mut() {
                    conn.close_after_flush = true;
                    conn.closing_since = tick;
                }
            }
            (Phase::Login, packet::login::sb::HELLO) => {
                let name = c.str(16)?.to_owned();
                self.login(idx, &name)?;
            }
            (Phase::Login, packet::login::sb::LOGIN_ACKNOWLEDGED) => {
                let conn = self.conns[idx].as_mut().expect("connection exists");
                if conn.player.is_none() {
                    return Err(invalid("login acknowledged before hello"));
                }
                conn.phase = Phase::Configuration;
                let compression = conn.compression;
                let mut p = PacketWriter::new(packet::configuration::cb::UPDATE_ENABLED_FEATURES);
                p.var_i32(1);
                p.string("minecraft:vanilla");
                self.send(idx, p.finish(compression));
                let mut p = PacketWriter::new(packet::configuration::cb::SELECT_KNOWN_PACKS);
                p.var_i32(1);
                p.string("minecraft");
                p.string("core");
                p.string(CLIENT_VERSION);
                self.send(idx, p.finish(compression));
            }
            (Phase::Configuration, packet::configuration::sb::CLIENT_INFORMATION)
            | (Phase::Play, packet::play::sb::CLIENT_INFORMATION) => {
                let _locale = c.str(16)?;
                let _view_distance = c.u8()?;
                let _chat_mode = c.var_i32()?;
                let _chat_colors = c.bool()?;
                let skin_parts = c.u8()?;
                let compression = self.config.compression;
                let player = self.player_mut(idx)?;
                player.skin_parts = skin_parts;
                if phase == Phase::Play {
                    let frame = entity_skin(player, compression);
                    self.broadcast(frame, None);
                }
            }
            (Phase::Configuration, packet::configuration::sb::SELECT_KNOWN_PACKS) => {
                let burst = self.configuration_burst.clone();
                self.send_shared(idx, &burst);
            }
            (Phase::Configuration, packet::configuration::sb::FINISH_CONFIGURATION) => {
                self.enter_play(idx);
            }
            (Phase::Play, id) => self.handle_play(idx, id, &mut c)?,
            _ => {}
        }
        Ok(())
    }

    fn login(&mut self, idx: usize, name: &str) -> Result<()> {
        if self.conns[idx].as_ref().is_some_and(|c| c.player.is_some()) {
            return Err(invalid("duplicate login"));
        }
        if self.online >= self.config.max_players {
            self.kick(idx, "The server is full");
            return Ok(());
        }
        let taken = self
            .conns
            .iter()
            .flatten()
            .any(|c| c.player.is_some() && c.name.eq_ignore_ascii_case(name));
        if !valid_name(name) || taken {
            self.kick(idx, "That name is invalid or already playing");
            return Ok(());
        }
        let (sx, sy, sz) = self.world.spawn_point();
        let player = Player {
            entity_id: self.next_entity_id,
            uuid: player_uuid(name),
            x: sx,
            y: sy,
            z: sz,
            yaw: 0.0,
            pitch: 0.0,
            on_ground: true,
            moved: false,
            rotated: false,
            center: (chunk_of(sx), chunk_of(sz)),
            chunk_queue: Vec::new(),
            keepalive_pending: false,
            chat_tokens: CHAT_BURST,
            skin_parts: 0x7f,
            sneaking: false,
            hotbar: 0,
            inventory: self.inventories.get(name).cloned().unwrap_or_default(),
            crafting: [Slot::default(); 9],
        };
        self.next_entity_id = self.next_entity_id.wrapping_add(1).max(1);
        let compression = self.config.compression;
        if compression {
            let mut p = PacketWriter::new(packet::login::cb::LOGIN_COMPRESSION);
            p.var_i32(COMPRESSION_THRESHOLD as i32);
            self.send(idx, p.finish(false));
        }
        let mut p = PacketWriter::new(packet::login::cb::LOGIN_FINISHED);
        p.uuid(player.uuid);
        p.string(name);
        p.var_i32(0);
        let frame = p.finish(compression);
        let conn = self.conns[idx].as_mut().expect("connection exists");
        conn.compression = compression;
        conn.name = name.to_owned();
        conn.player = Some(Box::new(player));
        self.send(idx, frame);
        Ok(())
    }

    fn enter_play(&mut self, idx: usize) {
        if self.online == 0 {
            self.metrics.reset();
            self.metrics_active = true;
            self.metrics_reported = false;
        }
        let compression = self.config.compression;
        let max_players = self.config.max_players;
        let seed = self.world.seed();
        let day_time = self.day_time;
        let Some(conn) = self.conns[idx].as_mut() else {
            return;
        };
        let Some(player) = conn.player.as_mut() else {
            return;
        };
        conn.phase = Phase::Play;
        player.chunk_queue = chunks_around(player.center);
        let me = PlayerView::of(player, &conn.name);
        let mut frames = vec![
            play_login(me.entity_id, max_players, seed, compression),
            set_default_spawn(me.x, me.y, me.z, compression),
            player_position(me.x, me.y, me.z, compression),
            chunk_center(player.center, compression),
            game_event(13, 0.0, compression),
            player_info_add(&me, true, compression),
            entity_skin(player, compression),
            container_set_content(&player.inventory, compression),
            set_time(day_time, compression),
        ];
        let mut announce = vec![
            player_info_add(&me, false, compression),
            add_player_entity(&me, compression),
            entity_skin(player, compression),
        ];
        let joined = system_chat(&format!("§e{} joined the game", me.name), compression);
        let name = me.name.to_owned();
        for (other_idx, other) in self.conns.iter().enumerate() {
            let Some(other) = other.as_ref().filter(|c| c.phase == Phase::Play) else {
                continue;
            };
            if other_idx == idx {
                continue;
            }
            let player = other.player.as_deref().expect("players in play have state");
            let view = PlayerView::of(player, &other.name);
            frames.push(player_info_add(&view, false, compression));
            frames.push(add_player_entity(&view, compression));
            frames.push(entity_skin(player, compression));
            if player.sneaking {
                frames.push(entity_pose(player, compression));
            }
        }
        self.online += 1;
        for frame in frames {
            self.send(idx, frame);
        }
        for frame in announce.drain(..) {
            self.broadcast(frame, Some(idx));
        }
        self.broadcast(joined, None);
        let online = self.online;
        self.info(format_args!("{name} joined ({online} online)"));
    }

    fn handle_play(&mut self, idx: usize, id: i32, c: &mut Cursor<'_>) -> Result<()> {
        use packet::play::sb;
        let compression = self.config.compression;
        match id {
            sb::CHAT => {
                let message = c.str(MAX_CHAT_CHARS)?;
                self.chat(idx, message);
            }
            sb::KEEP_ALIVE => {
                let _id = c.i64()?;
                self.player_mut(idx)?.keepalive_pending = false;
            }
            sb::MOVE_PLAYER_POS => {
                let pos = (c.f64()?, c.f64()?, c.f64()?);
                let flags = c.u8()?;
                self.movement(idx, Some(pos), None, flags)?;
            }
            sb::MOVE_PLAYER_POS_ROT => {
                let pos = (c.f64()?, c.f64()?, c.f64()?);
                let rot = (c.f32()?, c.f32()?);
                let flags = c.u8()?;
                self.movement(idx, Some(pos), Some(rot), flags)?;
            }
            sb::MOVE_PLAYER_ROT => {
                let rot = (c.f32()?, c.f32()?);
                let flags = c.u8()?;
                self.movement(idx, None, Some(rot), flags)?;
            }
            sb::MOVE_PLAYER_STATUS_ONLY => {
                let flags = c.u8()?;
                self.player_mut(idx)?.on_ground = flags & 1 != 0;
            }
            sb::PLAYER_ACTION => {
                let status = c.var_i32()?;
                let (x, y, z) = c.position()?;
                let _face = c.u8()?;
                let sequence = c.var_i32()?;
                if status == 2 {
                    self.break_block(idx, x, y, z);
                }
                self.send(idx, block_ack(sequence, compression));
            }
            sb::USE_ITEM_ON => {
                let hand = c.var_i32()?;
                let clicked = c.position()?;
                let face = c.var_i32()?;
                let _cursor = (c.f32()?, c.f32()?, c.f32()?);
                let _inside = c.bool()?;
                let _border = c.bool()?;
                let sequence = c.var_i32()?;
                self.use_item_on(idx, hand, clicked, face);
                self.send(idx, block_ack(sequence, compression));
            }
            sb::PLAYER_INPUT => {
                let sneaking = c.u8()? & 0x20 != 0;
                let player = self.player_mut(idx)?;
                if player.sneaking != sneaking {
                    player.sneaking = sneaking;
                    let frame = entity_pose(player, compression);
                    self.broadcast(frame, Some(idx));
                }
            }
            sb::SET_CARRIED_ITEM => {
                let slot = c.i16()?;
                if (0..9).contains(&slot) {
                    self.player_mut(idx)?.hotbar = slot as usize;
                }
            }
            sb::SWING => {
                let hand = c.var_i32()?;
                let entity_id = self.player_mut(idx)?.entity_id;
                let mut p = PacketWriter::new(packet::play::cb::ANIMATE);
                p.var_i32(entity_id);
                p.u8(if hand == 1 { 3 } else { 0 });
                self.broadcast(p.finish(compression), Some(idx));
            }
            sb::CONTAINER_CLICK => self.container_click(idx, c)?,
            sb::CONTAINER_CLOSE => {
                let window = c.var_i32()?;
                let player = self.player_mut(idx)?;
                if window == 1 {
                    for slot in 0..9 {
                        let item = std::mem::take(&mut player.crafting[slot]);
                        if item.count > 0 && item.item_id != AIR_ITEM {
                            player.inventory.insert(item.item_id, item.count);
                        }
                    }
                } else {
                    for slot in 1..=4 {
                        let item = player.inventory.get(slot);
                        player.inventory.set(slot, 0, AIR_ITEM);
                        if item.count > 0 && item.item_id != AIR_ITEM {
                            player.inventory.insert(item.item_id, item.count);
                        }
                    }
                    player.inventory.set(0, 0, AIR_ITEM);
                }
                let frame = container_set_content(&player.inventory, compression);
                self.send(idx, frame);
            }
            _ => {}
        }
        Ok(())
    }

    fn chat(&mut self, idx: usize, message: &str) {
        let allowed = !message.trim().is_empty()
            && message
                .chars()
                .all(|ch| ch >= ' ' && ch != '\u{7f}' && ch != '§');
        let Some(conn) = self.conns[idx].as_mut() else {
            return;
        };
        let Some(player) = conn.player.as_mut() else {
            return;
        };
        if !allowed || player.chat_tokens == 0 {
            return;
        }
        player.chat_tokens -= 1;
        let line = format!("<{}> {message}", conn.name);
        self.info(format_args!("{line}"));
        self.broadcast(system_chat(&line, self.config.compression), None);
    }

    fn movement(
        &mut self,
        idx: usize,
        pos: Option<(f64, f64, f64)>,
        rot: Option<(f32, f32)>,
        flags: u8,
    ) -> Result<()> {
        let compression = self.config.compression;
        let player = self.player_mut(idx)?;
        if let Some((x, y, z)) = pos {
            if !(x.is_finite() && y.is_finite() && z.is_finite())
                || x.abs() > 3.0e7
                || z.abs() > 3.0e7
                || y.abs() > 2.0e7
            {
                return Err(invalid("invalid position"));
            }
            let (dx, dy, dz) = (x - player.x, y - player.y, z - player.z);
            if dx * dx + dy * dy + dz * dz > MAX_MOVE_PER_PACKET * MAX_MOVE_PER_PACKET {
                let (px, py, pz) = (player.x, player.y, player.z);
                self.send(idx, player_position(px, py, pz, compression));
                return Ok(());
            }
            if (x, y, z) != (player.x, player.y, player.z) {
                player.x = x;
                player.y = y;
                player.z = z;
                player.moved = true;
            }
        }
        if let Some((yaw, pitch)) = rot {
            if !(yaw.is_finite() && pitch.is_finite()) {
                return Err(invalid("invalid rotation"));
            }
            if (yaw, pitch) != (player.yaw, player.pitch) {
                player.yaw = yaw;
                player.pitch = pitch;
                player.rotated = true;
            }
        }
        player.on_ground = flags & 1 != 0;
        let center = (chunk_of(player.x), chunk_of(player.z));
        if center != player.center {
            let old = player.center;
            player.center = center;
            retarget_chunk_queue(&mut player.chunk_queue, old, center);
            self.send(idx, chunk_center(center, compression));
        }
        Ok(())
    }

    fn break_block(&mut self, idx: usize, x: i32, y: i32, z: i32) {
        let Ok(player) = self.player_mut(idx) else {
            return;
        };
        if !within_reach(player, x, y, z) {
            return;
        }
        let held = player.inventory.get(36 + player.hotbar).item_id;
        let current = self.world.block(x, y, z);
        if current == block::AIR || current == block::BEDROCK || current == block::WATER {
            self.resync_block(idx, x, y, z);
            return;
        }
        let roll = self.roll();
        if !self.set_block(x, y, z, block::AIR) {
            return;
        }
        let compression = self.config.compression;
        let Ok(player) = self.player_mut(idx) else {
            return;
        };
        let mut gained = false;
        // Sculk has no item entities: drops go straight to the inventory, and
        // anything that does not fit is lost.
        for_each_drop(current, held, roll, |item, count| {
            if item != AIR_ITEM && count > 0 {
                player.inventory.insert(item, count);
                gained = true;
            }
        });
        if gained {
            let frame = container_set_content(&player.inventory, compression);
            self.send(idx, frame);
        }
    }

    fn use_item_on(&mut self, idx: usize, hand: i32, clicked: (i32, i32, i32), face: i32) {
        let compression = self.config.compression;
        let Ok(player) = self.player_mut(idx) else {
            return;
        };
        if !within_reach(player, clicked.0, clicked.1, clicked.2) {
            return;
        }
        let sneaking = player.sneaking;
        let clicked_block = self.world.block(clicked.0, clicked.1, clicked.2);
        let Ok(player) = self.player_mut(idx) else {
            return;
        };
        if clicked_block == block::CRAFTING_TABLE && !sneaking {
            let mut p = PacketWriter::new(packet::play::cb::OPEN_SCREEN);
            p.var_i32(1);
            p.var_i32(vanilla::menu::CRAFTING);
            p.nbt_text("Crafting");
            self.send(idx, p.finish(compression));
            return;
        }
        let held_slot = if hand == 1 { 45 } else { 36 + player.hotbar };
        let held = player.inventory.get(held_slot);
        let state = block_state_from_item(held.item_id);
        let target = if replaceable(clicked_block) {
            clicked
        } else {
            let (dx, dy, dz) = match face {
                0 => (0, -1, 0),
                1 => (0, 1, 0),
                2 => (0, 0, -1),
                3 => (0, 0, 1),
                4 => (-1, 0, 0),
                _ => (1, 0, 0),
            };
            (clicked.0 + dx, clicked.1 + dy, clicked.2 + dz)
        };
        let (x, y, z) = target;
        let reachable = within_reach(player, x, y, z);
        let valid = held.count > 0
            && state != block::AIR
            && reachable
            && (MIN_Y..MIN_Y + 16 * SECTION_COUNT as i32).contains(&y)
            && replaceable(self.world.block(x, y, z))
            && !self.player_occupies(x, y, z);
        if !valid {
            self.resync_block(idx, x, y, z);
            return;
        }
        if self.set_block(x, y, z, state) {
            if let Ok(player) = self.player_mut(idx) {
                player
                    .inventory
                    .set(held_slot, held.count - 1, held.item_id);
                let frame = container_set_content(&player.inventory, compression);
                self.send(idx, frame);
            }
        }
    }

    fn container_click(&mut self, idx: usize, c: &mut Cursor<'_>) -> Result<()> {
        let window = c.var_i32()?;
        let _state = c.var_i32()?;
        let _slot = c.i16()?;
        let _button = c.u8()?;
        let _mode = c.var_i32()?;
        let changed = c.var_i32()?;
        if !(0..=128).contains(&changed) {
            return Err(invalid("too many changed slots"));
        }
        let mut grid_changed = false;
        for _ in 0..changed {
            let slot = c.i16()?;
            let item = read_hashed_slot(c)?;
            let player = self.player_mut(idx)?;
            match (window, slot) {
                (0, 1..=4) => {
                    player
                        .inventory
                        .set(slot as usize, item.count, item.item_id);
                    grid_changed = true;
                }
                (0, 0) | (1, 0) => grid_changed = true,
                (0, 5..=45) => player
                    .inventory
                    .set(slot as usize, item.count, item.item_id),
                (1, 1..=9) => {
                    player.crafting[slot as usize - 1] = item;
                    grid_changed = true;
                }
                (1, 10..=45) => player
                    .inventory
                    .set(slot as usize - 1, item.count, item.item_id),
                _ => {}
            }
        }
        if grid_changed {
            let compression = self.config.compression;
            let player = self.player_mut(idx)?;
            let mut grid = [Slot::default(); 9];
            if window == 0 {
                for (dst, src) in [(0, 1), (1, 2), (3, 3), (4, 4)] {
                    grid[dst] = player.inventory.get(src);
                }
            } else {
                grid = player.crafting;
            }
            let result = crafted_item(&grid);
            if window == 0 {
                player.inventory.set(0, result.count, result.item_id);
            }
            let mut p = PacketWriter::new(packet::play::cb::CONTAINER_SET_SLOT);
            p.var_i32(window);
            p.var_i32(0);
            p.i16(0);
            write_slot(&mut p, result);
            self.send(idx, p.finish(compression));
        }
        Ok(())
    }

    // ------------------------------------------------------------------- tick

    fn tick(&mut self) {
        self.tick += 1;
        self.day_time = (self.day_time + 1) % 24_000;
        self.stream_chunks();
        self.broadcast_movement();
        let compression = self.config.compression;
        let tick = self.tick;

        let mut kicks: Vec<(usize, &'static str)> = Vec::new();
        for (idx, conn) in self.conns.iter_mut().enumerate() {
            let Some(conn) = conn.as_mut() else { continue };
            if conn.close_after_flush {
                if tick - conn.closing_since > CLOSE_GRACE_TICKS {
                    kicks.push((idx, ""));
                }
                continue;
            }
            if let Some(reason) = conn.kick {
                kicks.push((idx, reason));
                continue;
            }
            if conn.phase != Phase::Play {
                if tick - conn.connected_tick > LOGIN_TIMEOUT_TICKS {
                    kicks.push((idx, "Login timed out"));
                }
                continue;
            }
            let player = conn.player.as_mut().expect("players in play have state");
            if tick % CHAT_REFILL_TICKS == 0 && player.chat_tokens < CHAT_BURST {
                player.chat_tokens += 1;
            }
            if tick % KEEPALIVE_TICKS == 0 {
                if player.keepalive_pending {
                    kicks.push((idx, "Timed out"));
                } else {
                    player.keepalive_pending = true;
                }
            }
        }
        for (idx, reason) in kicks {
            self.kick(idx, reason);
        }
        if tick % KEEPALIVE_TICKS == 0 {
            let mut p = PacketWriter::new(packet::play::cb::KEEP_ALIVE);
            p.i64(tick as i64);
            self.broadcast(p.finish(compression), None);
        }
        if tick % TIME_SYNC_TICKS == 0 {
            self.broadcast(set_time(self.day_time, compression), None);
        }
        if tick % SAVE_TICKS == 0 && self.world_dirty {
            self.save();
        }
        for idx in 0..self.conns.len() {
            if self.conns[idx].as_ref().is_some_and(|c| !c.tx.is_empty()) {
                self.flush(idx);
            }
        }
        let _ = self.log.flush();
    }

    fn stream_chunks(&mut self) {
        for idx in 0..self.conns.len() {
            let Some(player) = self
                .conns
                .get_mut(idx)
                .and_then(Option::as_mut)
                .filter(|c| c.phase == Phase::Play)
                .and_then(|c| c.player.as_mut())
            else {
                continue;
            };
            if player.chunk_queue.is_empty() {
                continue;
            }
            let mut queue = std::mem::take(&mut player.chunk_queue);
            for _ in 0..CHUNKS_PER_TICK {
                let Some(key) = queue.pop() else { break };
                let frame = self.chunk_frame(key);
                self.send_shared(idx, &frame);
            }
            if let Some(player) = self.conns[idx].as_mut().and_then(|c| c.player.as_mut()) {
                player.chunk_queue = queue;
            }
        }
    }

    fn chunk_frame(&mut self, key: (i32, i32)) -> Rc<Frame> {
        if let Some(frame) = self.chunks.get(key) {
            return frame;
        }
        let started = Instant::now();
        let allocations = crate::allocation_metrics::snapshot();
        let mut p = PacketWriter::with_capacity(
            packet::play::cb::LEVEL_CHUNK_WITH_LIGHT,
            LEVEL_CHUNK_SIZE_HINT,
        );
        write_level_chunk(&self.world, key.0, key.1, p.buffer());
        let frame = Rc::new(p.finish(self.config.compression));
        if self.metrics_active {
            self.metrics.record_chunk(started.elapsed(), allocations);
        }
        self.chunks.insert(key, frame.clone());
        frame
    }

    /// Encodes this tick's movement once into a shared buffer; each recipient
    /// is sent that buffer minus its own segment.
    fn broadcast_movement(&mut self) {
        let compression = self.config.compression;
        let mut buf = Vec::new();
        let mut segments: Vec<(usize, u32, u32)> = Vec::new();
        for (idx, conn) in self.conns.iter_mut().enumerate() {
            let Some(player) = conn
                .as_mut()
                .filter(|c| c.phase == Phase::Play)
                .and_then(|c| c.player.as_mut())
            else {
                continue;
            };
            if !player.moved && !player.rotated {
                continue;
            }
            let start = buf.len() as u32;
            if player.moved {
                let mut p = PacketWriter::new(packet::play::cb::ENTITY_POSITION_SYNC);
                p.var_i32(player.entity_id);
                for v in [player.x, player.y, player.z, 0.0, 0.0, 0.0] {
                    p.f64(v);
                }
                p.f32(player.yaw);
                p.f32(player.pitch);
                p.bool(player.on_ground);
                buf.extend_from_slice(&p.finish(compression));
            } else {
                let mut p = PacketWriter::new(packet::play::cb::MOVE_ENTITY_ROT);
                p.var_i32(player.entity_id);
                p.u8(angle(player.yaw));
                p.u8(angle(player.pitch));
                p.bool(player.on_ground);
                buf.extend_from_slice(&p.finish(compression));
            }
            let mut p = PacketWriter::new(packet::play::cb::ROTATE_HEAD);
            p.var_i32(player.entity_id);
            p.u8(angle(player.yaw));
            buf.extend_from_slice(&p.finish(compression));
            player.moved = false;
            player.rotated = false;
            segments.push((idx, start, buf.len() as u32));
        }
        if segments.is_empty() {
            return;
        }
        let len = buf.len() as u32;
        let shared = Rc::new(Frame::from_vec(buf));
        let mut own = segments.iter().peekable();
        for idx in 0..self.conns.len() {
            if !self.in_play(idx) {
                continue;
            }
            match own.peek() {
                Some(&&(sender, start, end)) if sender == idx => {
                    own.next();
                    self.send_slice(idx, &shared, 0, start);
                    self.send_slice(idx, &shared, end, len);
                }
                _ => self.send_slice(idx, &shared, 0, len),
            }
        }
    }

    // ---------------------------------------------------------------- helpers

    fn set_block(&mut self, x: i32, y: i32, z: i32, state: i32) -> bool {
        if !self.world.set_block(x, y, z, state) {
            return false;
        }
        self.world_dirty = true;
        self.chunks.invalidate((x >> 4, z >> 4));
        self.broadcast(block_update(x, y, z, state, self.config.compression), None);
        true
    }

    fn resync_block(&mut self, idx: usize, x: i32, y: i32, z: i32) {
        let state = self.world.block(x, y, z);
        let frame = block_update(x, y, z, state, self.config.compression);
        self.send(idx, frame);
    }

    fn player_occupies(&self, x: i32, y: i32, z: i32) -> bool {
        let (bx, by, bz) = (f64::from(x), f64::from(y), f64::from(z));
        self.conns
            .iter()
            .flatten()
            .filter(|c| c.phase == Phase::Play)
            .filter_map(|c| c.player.as_deref())
            .any(|p| {
                p.x + 0.3 > bx
                    && p.x - 0.3 < bx + 1.0
                    && p.z + 0.3 > bz
                    && p.z - 0.3 < bz + 1.0
                    && p.y + 1.8 > by
                    && p.y < by + 1.0
            })
    }

    fn roll(&mut self) -> u32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 32) as u32
    }

    fn save(&mut self) {
        let Some(path) = self.config.world_path.clone() else {
            self.world_dirty = false;
            return;
        };
        let mut inventories = self.inventories.clone();
        for conn in self.conns.iter().flatten() {
            if let Some(player) = &conn.player {
                inventories.insert(conn.name.clone(), player.inventory.clone());
            }
        }
        let data = SaveData {
            seed: self.world.seed(),
            edits: self.world.edits().collect(),
            inventories: inventories.into_iter().collect(),
        };
        match save::store(&path, &data) {
            Ok(()) => self.world_dirty = false,
            Err(err) => self.warn(format_args!("could not save {}: {err}", path.display())),
        }
    }

    fn in_play(&self, idx: usize) -> bool {
        self.conns[idx]
            .as_ref()
            .is_some_and(|c| c.phase == Phase::Play)
    }

    fn player_mut(&mut self, idx: usize) -> Result<&mut Player> {
        self.conns
            .get_mut(idx)
            .and_then(Option::as_mut)
            .and_then(|c| c.player.as_deref_mut())
            .ok_or_else(|| invalid("packet requires a logged-in player"))
    }

    fn conn_label(&self, idx: usize) -> String {
        match self.conns.get(idx).and_then(Option::as_ref) {
            Some(conn) if !conn.name.is_empty() => conn.name.clone(),
            Some(conn) => match conn.socket.peer_addr() {
                Ok(addr) => addr.to_string(),
                Err(_) => format!("connection {idx}"),
            },
            None => format!("connection {idx}"),
        }
    }

    fn info(&mut self, args: Arguments<'_>) {
        log_line(&mut self.log, "INFO", args);
    }

    fn warn(&mut self, args: Arguments<'_>) {
        log_line(&mut self.log, "WARN", args);
    }
}

// -------------------------------------------------------------------- helpers

struct PlayerView<'a> {
    entity_id: i32,
    uuid: u128,
    name: &'a str,
    x: f64,
    y: f64,
    z: f64,
    yaw: f32,
    pitch: f32,
}

impl<'a> PlayerView<'a> {
    fn of(player: &Player, name: &'a str) -> Self {
        Self {
            entity_id: player.entity_id,
            uuid: player.uuid,
            name,
            x: player.x,
            y: player.y,
            z: player.z,
            yaw: player.yaw,
            pitch: player.pitch,
        }
    }
}

/// Kernel socket buffers hold a private copy of every byte queued for one
/// client, while Sculk's own queue shares one encoded copy between all
/// recipients. Small kernel buffers keep pending data in the shared queue,
/// which bounds memory per connection (and the cgroup charge for it).
const SOCKET_SEND_BUFFER: i32 = 16 * 1024;
const SOCKET_RECV_BUFFER: i32 = 4 * 1024;

#[cfg(unix)]
fn limit_kernel_buffers(socket: &TcpStream) {
    use std::os::fd::AsRawFd;
    for (option, size) in [
        (libc::SO_SNDBUF, SOCKET_SEND_BUFFER),
        (libc::SO_RCVBUF, SOCKET_RECV_BUFFER),
    ] {
        // SAFETY: the descriptor is a valid open socket owned by `socket`, and
        // the option value is a live i32 of the length passed.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&size as *const i32).cast(),
                std::mem::size_of::<i32>() as libc::socklen_t,
            );
        }
    }
}

#[cfg(not(unix))]
fn limit_kernel_buffers(_socket: &TcpStream) {}

fn invalid(message: &'static str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

fn chunk_of(coord: f64) -> i32 {
    (coord.floor() as i64 >> 4) as i32
}

/// All chunks within view distance of `center`, nearest last.
fn chunks_around(center: (i32, i32)) -> Vec<(i32, i32)> {
    let side = (2 * VIEW_DISTANCE + 1) as usize;
    let mut chunks = Vec::with_capacity(side * side);
    for dz in -VIEW_DISTANCE..=VIEW_DISTANCE {
        for dx in -VIEW_DISTANCE..=VIEW_DISTANCE {
            chunks.push((center.0 + dx, center.1 + dz));
        }
    }
    sort_far_to_near(&mut chunks, center);
    chunks
}

/// Drops queued chunks that left view and adds the newly visible ones.
fn retarget_chunk_queue(queue: &mut Vec<(i32, i32)>, old: (i32, i32), new: (i32, i32)) {
    let visible = |c: (i32, i32), center: (i32, i32)| {
        (c.0 - center.0).abs() <= VIEW_DISTANCE && (c.1 - center.1).abs() <= VIEW_DISTANCE
    };
    queue.retain(|&c| visible(c, new));
    for chunk in chunks_around(new) {
        if !visible(chunk, old) && !queue.contains(&chunk) {
            queue.push(chunk);
        }
    }
    sort_far_to_near(queue, new);
}

fn sort_far_to_near(chunks: &mut [(i32, i32)], center: (i32, i32)) {
    chunks.sort_unstable_by_key(|&(x, z)| {
        let (dx, dz) = (x - center.0, z - center.1);
        std::cmp::Reverse(dx * dx + dz * dz)
    });
}

fn within_reach(player: &Player, x: i32, y: i32, z: i32) -> bool {
    let dx = f64::from(x) + 0.5 - player.x;
    let dy = f64::from(y) + 0.5 - (player.y + EYE_HEIGHT);
    let dz = f64::from(z) + 0.5 - player.z;
    dx * dx + dy * dy + dz * dz <= REACH_SQ
}

fn replaceable(state: i32) -> bool {
    matches!(
        state,
        block::AIR | block::WATER | block::SHORT_GRASS | block::FERN | block::SNOW
    )
}

/// Stable per-name UUID (RFC 9562 version 8, custom layout).
fn player_uuid(name: &str) -> u128 {
    let mut hash = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58du128;
    for byte in name.bytes() {
        hash ^= u128::from(byte.to_ascii_lowercase());
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    hash ^= hash >> 67;
    (hash & !(0xf << 76) & !(0x3 << 62)) | (0x8 << 76) | (0x2 << 62)
}

fn angle(degrees: f32) -> u8 {
    (degrees.rem_euclid(360.0) * (256.0 / 360.0)) as u8
}

fn read_hashed_slot(c: &mut Cursor<'_>) -> Result<Slot> {
    if !c.bool()? {
        return Ok(Slot::default());
    }
    let item_id = c.var_i32()?;
    let count = c.var_i32()?;
    let added = c.var_i32()?;
    let removed = c.var_i32()?;
    if added != 0 || removed != 0 {
        return Err(invalid("item components are not supported"));
    }
    if !(0..=99).contains(&count) {
        return Err(invalid("invalid item count"));
    }
    Ok(Slot {
        count: count as i16,
        item_id,
    })
}

fn write_slot(p: &mut PacketWriter, slot: Slot) {
    if slot.count <= 0 || slot.item_id == AIR_ITEM {
        p.var_i32(0);
        return;
    }
    p.var_i32(i32::from(slot.count));
    p.var_i32(slot.item_id);
    p.var_i32(0);
    p.var_i32(0);
}

fn log_line(log: &mut BufWriter<Stdout>, level: &str, args: Arguments<'_>) {
    let _ = writeln!(log, "[{level}] {args}");
}

fn log_runtime_metrics(log: &mut BufWriter<Stdout>, snapshot: RuntimeSnapshot) {
    let elapsed = snapshot.elapsed.as_secs_f64().max(f64::EPSILON);
    let chunks = snapshot.chunk.count();
    let _ = writeln!(
        log,
        "[METRICS]: metrics_elapsed_secs={elapsed:.6} tick_count={} tick_avg_us={:.3} tick_p95_us={:.3} tick_p99_us={:.3} tick_max_us={:.3} chunk_count={chunks} chunk_avg_us={:.3} chunk_p95_us={:.3} chunk_p99_us={:.3} chunk_max_us={:.3} allocation_metrics={} allocation_calls={} allocation_bytes={} allocation_calls_per_sec={:.3} heap_live_bytes={} heap_peak_bytes={} chunk_allocation_calls={} chunk_allocation_bytes={} allocations_per_chunk={:.6} allocation_bytes_per_chunk={:.3}",
        snapshot.tick.count(),
        snapshot.tick.average_us(),
        snapshot.tick.percentile_us(0.95),
        snapshot.tick.percentile_us(0.99),
        snapshot.tick.max_us(),
        snapshot.chunk.average_us(),
        snapshot.chunk.percentile_us(0.95),
        snapshot.chunk.percentile_us(0.99),
        snapshot.chunk.max_us(),
        crate::allocation_metrics::ENABLED,
        snapshot.allocations.calls,
        snapshot.allocations.bytes,
        snapshot.allocations.calls as f64 / elapsed,
        crate::allocation_metrics::snapshot().live,
        crate::allocation_metrics::snapshot().peak,
        snapshot.chunk_allocations.calls,
        snapshot.chunk_allocations.bytes,
        snapshot.chunk_allocations.calls as f64 / chunks.max(1) as f64,
        snapshot.chunk_allocations.bytes as f64 / chunks.max(1) as f64,
    );
    let _ = log.flush();
}

// ------------------------------------------------------------ packet builders

fn status_response(motd: &str, max_players: usize, online: usize) -> Frame {
    let mut description = String::new();
    for ch in motd.chars() {
        match ch {
            '"' => description.push_str("\\\""),
            '\\' => description.push_str("\\\\"),
            c if (c as u32) < 0x20 => description.push(' '),
            c => description.push(c),
        }
    }
    let mut p = PacketWriter::new(packet::status::cb::STATUS_RESPONSE);
    p.string(&format!(
        r#"{{"version":{{"name":"{CLIENT_VERSION}","protocol":{PROTOCOL_VERSION}}},"players":{{"max":{max_players},"online":{online}}},"description":{{"text":"{description}"}}}}"#
    ));
    p.finish(false)
}

fn login_disconnect(reason: &str) -> Frame {
    let mut p = PacketWriter::new(packet::login::cb::LOGIN_DISCONNECT);
    p.json_text(reason);
    p.finish(false)
}

/// Registry data, tags, and the end of configuration, encoded once at startup.
fn configuration_burst(compression: bool) -> Frame {
    let mut out = Vec::new();
    for (registry, entries) in vanilla::REGISTRIES {
        let mut p = PacketWriter::new(packet::configuration::cb::REGISTRY_DATA);
        p.string(registry);
        p.var_i32(entries.len() as i32);
        for entry in *entries {
            p.string(entry);
            p.bool(false);
        }
        out.extend_from_slice(&p.finish(compression));
    }
    let mut p = PacketWriter::with_capacity(packet::configuration::cb::UPDATE_TAGS, 64 * 1024);
    p.var_i32(vanilla::TAGS.len() as i32);
    for (registry, tags) in vanilla::TAGS {
        p.string(registry);
        p.var_i32(tags.len() as i32);
        for (tag, members) in *tags {
            p.string(tag);
            p.var_i32(members.len() as i32);
            for member in *members {
                p.var_i32(*member);
            }
        }
    }
    out.extend_from_slice(&p.finish(compression));
    out.extend_from_slice(
        &PacketWriter::new(packet::configuration::cb::FINISH_CONFIGURATION).finish(compression),
    );
    Frame::from_vec(out)
}

fn play_login(entity_id: i32, max_players: usize, seed: u64, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::LOGIN);
    p.i32(entity_id);
    p.bool(false); // hardcore
    p.var_i32(1);
    p.string("overworld");
    p.var_i32(max_players.min(i32::MAX as usize) as i32);
    p.var_i32(VIEW_DISTANCE);
    p.var_i32(VIEW_DISTANCE);
    p.bool(false); // reduced debug info
    p.bool(true); // respawn screen
    p.bool(false); // limited crafting
    p.var_i32(vanilla::dimension_type::OVERWORLD);
    p.string("overworld");
    p.u64(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(31));
    p.u8(0); // survival
    p.u8(0xff); // no previous game mode
    p.bool(false); // debug world
    p.bool(false); // flat world
    p.bool(false); // no death location
    p.var_i32(0); // portal cooldown
    p.var_i32(SEA_LEVEL);
    p.bool(false); // secure chat
    p.finish(compression)
}

fn set_default_spawn(x: f64, y: f64, z: f64, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::SET_DEFAULT_SPAWN_POSITION);
    p.string("overworld");
    p.position(x.floor() as i32, y.floor() as i32, z.floor() as i32);
    p.f32(0.0);
    p.f32(0.0);
    p.finish(compression)
}

fn player_position(x: f64, y: f64, z: f64, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::PLAYER_POSITION);
    p.var_i32(1);
    for v in [x, y, z, 0.0, 0.0, 0.0] {
        p.f64(v);
    }
    p.f32(0.0);
    p.f32(0.0);
    p.i32(0);
    p.finish(compression)
}

fn chunk_center((x, z): (i32, i32), compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::SET_CHUNK_CACHE_CENTER);
    p.var_i32(x);
    p.var_i32(z);
    p.finish(compression)
}

fn game_event(event: u8, value: f32, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::GAME_EVENT);
    p.u8(event);
    p.f32(value);
    p.finish(compression)
}

fn player_info_add(player: &PlayerView<'_>, with_latency: bool, compression: bool) -> Frame {
    const ADD_PLAYER: u8 = 0x01;
    const UPDATE_LISTED: u8 = 0x08;
    const UPDATE_LATENCY: u8 = 0x10;
    let mut p = PacketWriter::new(packet::play::cb::PLAYER_INFO_UPDATE);
    p.u8(ADD_PLAYER | UPDATE_LISTED | if with_latency { UPDATE_LATENCY } else { 0 });
    p.var_i32(1);
    p.uuid(player.uuid);
    p.string(player.name);
    p.var_i32(0); // properties
    p.bool(true); // listed
    if with_latency {
        p.var_i32(0);
    }
    p.finish(compression)
}

fn add_player_entity(player: &PlayerView<'_>, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::ADD_ENTITY);
    p.var_i32(player.entity_id);
    p.uuid(player.uuid);
    p.var_i32(vanilla::entity_type::PLAYER);
    p.f64(player.x);
    p.f64(player.y);
    p.f64(player.z);
    p.u8(0); // zero velocity
    p.u8(angle(player.pitch));
    p.u8(angle(player.yaw));
    p.u8(angle(player.yaw));
    p.var_i32(0);
    p.finish(compression)
}

fn entity_skin(player: &Player, compression: bool) -> Frame {
    const SKIN_PARTS_INDEX: u8 = 16;
    const BYTE_SERIALIZER: i32 = 0;
    let mut p = PacketWriter::new(packet::play::cb::SET_ENTITY_DATA);
    p.var_i32(player.entity_id);
    p.u8(SKIN_PARTS_INDEX);
    p.var_i32(BYTE_SERIALIZER);
    p.u8(player.skin_parts);
    p.u8(0xff);
    p.finish(compression)
}

fn entity_pose(player: &Player, compression: bool) -> Frame {
    const POSE_INDEX: u8 = 6;
    const POSE_SERIALIZER: i32 = 20;
    const STANDING: i32 = 0;
    const CROUCHING: i32 = 5;
    let mut p = PacketWriter::new(packet::play::cb::SET_ENTITY_DATA);
    p.var_i32(player.entity_id);
    p.u8(POSE_INDEX);
    p.var_i32(POSE_SERIALIZER);
    p.var_i32(if player.sneaking { CROUCHING } else { STANDING });
    p.u8(0xff);
    p.finish(compression)
}

fn container_set_content(inventory: &Inventory, compression: bool) -> Frame {
    let mut p = PacketWriter::with_capacity(packet::play::cb::CONTAINER_SET_CONTENT, 256);
    p.var_i32(0);
    p.var_i32(0);
    p.var_i32(INVENTORY_SIZE as i32);
    for slot in inventory.slots {
        write_slot(&mut p, slot);
    }
    p.var_i32(0); // carried item
    p.finish(compression)
}

fn system_chat(message: &str, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::SYSTEM_CHAT);
    p.nbt_text(message);
    p.bool(false);
    p.finish(compression)
}

fn set_time(day_time: i64, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::SET_TIME);
    p.i64(day_time);
    p.bool(false);
    p.finish(compression)
}

fn block_update(x: i32, y: i32, z: i32, state: i32, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::BLOCK_UPDATE);
    p.position(x, y, z);
    p.var_i32(state);
    p.finish(compression)
}

fn block_ack(sequence: i32, compression: bool) -> Frame {
    let mut p = PacketWriter::new(packet::play::cb::BLOCK_CHANGED_ACK);
    p.var_i32(sequence);
    p.finish(compression)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(frame: &Frame) -> &[u8] {
        let mut pos = 0;
        read_var_i32_from_slice(frame, &mut pos).unwrap();
        &frame[pos..]
    }

    #[test]
    fn set_time_wire_shape() {
        assert_eq!(
            body(&set_time(6000, false)),
            &[0x71, 0, 0, 0, 0, 0, 0, 0x17, 0x70, 0]
        );
    }

    #[test]
    fn chunk_queue_is_nearest_first_and_retargets() {
        let mut queue = chunks_around((0, 0));
        assert_eq!(queue.len(), 25);
        assert_eq!(queue.last(), Some(&(0, 0)));
        queue.truncate(20);
        retarget_chunk_queue(&mut queue, (0, 0), (1, 0));
        assert!(queue
            .iter()
            .all(|&(x, z)| (x - 1).abs() <= 2 && z.abs() <= 2));
        for z in -2..=2 {
            assert!(queue.contains(&(3, z)));
        }
        let nearest = *queue.last().unwrap();
        assert!((nearest.0 - 1).abs() + nearest.1.abs() <= 2);
    }

    #[test]
    fn uuids_are_stable_versioned_and_case_insensitive() {
        let a = player_uuid("Steve");
        assert_eq!(a, player_uuid("steve"));
        assert_ne!(a, player_uuid("Alex"));
        assert_eq!((a >> 76) & 0xf, 8);
        assert_eq!((a >> 62) & 0x3, 2);
    }

    #[test]
    fn angles_wrap() {
        assert_eq!(angle(0.0), 0);
        assert_eq!(angle(90.0), 64);
        assert_eq!(angle(-90.0), 192);
        assert_eq!(angle(450.0), 64);
    }

    #[test]
    fn hashed_slot_decoder_survives_arbitrary_bytes() {
        let mut state = 0x0bad_5eed_1234_5678u64;
        for len in 0..64 {
            for _ in 0..128 {
                let bytes: Vec<u8> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(11);
                        (state >> 33) as u8
                    })
                    .collect();
                let _ = read_hashed_slot(&mut Cursor::new(&bytes));
            }
        }
    }

    #[test]
    fn configuration_burst_is_well_formed() {
        for compression in [false, true] {
            let burst = configuration_burst(compression);
            let mut pos = 0;
            let mut frames = 0;
            while next_frame(&burst, &mut pos).unwrap().is_some() {
                frames += 1;
            }
            assert_eq!(pos, burst.len());
            assert_eq!(frames, vanilla::REGISTRIES.len() + 2);
        }
    }
}
