use anyhow::{bail, Result};
use clap::{Parser, ValueEnum};
use flate2::read::ZlibDecoder;
use serde::Serialize;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: i32 = 775;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Synthetic Minecraft load tester for Sculk-compatible servers"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 25565)]
    port: u16,
    #[arg(long, default_value_t = 1)]
    players: usize,
    #[arg(long, default_value_t = 30)]
    duration: u64,
    #[arg(long, value_enum, default_value_t = Workload::Idle)]
    workload: Workload,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "Sculk")]
    name_prefix: String,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Workload {
    Idle,
    Walk,
    Explore,
    ChunkHeavy,
    BlockHeavy,
    ChatHeavy,
    Mixed,
    /// Exercises every gameplay feature: placement, breaking, crafting, travel, chat.
    Gameplay,
}

#[derive(Default)]
struct Counters {
    connected: AtomicU64,
    login_ok: AtomicU64,
    packets_sent: AtomicU64,
    packets_recv: AtomicU64,
    bytes_sent: AtomicU64,
    bytes_recv: AtomicU64,
    errors: AtomicU64,
    compressed_packets_recv: AtomicU64,
    decompressed_bytes_recv: AtomicU64,
    chunks_recv: AtomicU64,
    chunk_parse_errors: AtomicU64,
    system_chat_recv: AtomicU64,
    component_parse_errors: AtomicU64,
    set_time_recv: AtomicU64,
    entity_sync_recv: AtomicU64,
    entity_rotation_recv: AtomicU64,
    head_rotation_recv: AtomicU64,
    block_update_recv: AtomicU64,
    block_ack_recv: AtomicU64,
    inventory_content_recv: AtomicU64,
    container_slot_recv: AtomicU64,
}

#[derive(Serialize)]
struct Report {
    players: usize,
    duration_secs: u64,
    workload: String,
    connected: u64,
    login_ok: u64,
    errors: u64,
    packets_sent: u64,
    packets_recv: u64,
    bytes_sent: u64,
    bytes_recv: u64,
    compressed_packets_recv: u64,
    decompressed_bytes_recv: u64,
    chunks_recv: u64,
    chunk_parse_errors: u64,
    system_chat_recv: u64,
    component_parse_errors: u64,
    set_time_recv: u64,
    status_ok: bool,
    entity_sync_recv: u64,
    entity_rotation_recv: u64,
    head_rotation_recv: u64,
    block_update_recv: u64,
    block_ack_recv: u64,
    inventory_content_recv: u64,
    container_slot_recv: u64,
    packets_per_sec: f64,
    bytes_per_sec: f64,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let counters = Arc::new(Counters::default());
    let addr = format!("{}:{}", args.host, args.port);
    status_probe(&addr)?;
    let stop_at = Instant::now() + Duration::from_secs(args.duration);
    let started = Instant::now();

    thread::scope(|scope| {
        for id in 0..args.players {
            let addr = addr.clone();
            let counters = counters.clone();
            let workload = args.workload;
            let name_prefix = args.name_prefix.clone();
            scope.spawn(move || {
                if let Err(err) = bot(id, &addr, &name_prefix, workload, stop_at, &counters) {
                    eprintln!("bot{id}: {err}");
                    counters.errors.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    let report = Report {
        players: args.players,
        duration_secs: args.duration,
        workload: args
            .workload
            .to_possible_value()
            .map(|v| v.get_name().to_owned())
            .unwrap_or_default(),
        connected: counters.connected.load(Ordering::Relaxed),
        login_ok: counters.login_ok.load(Ordering::Relaxed),
        errors: counters.errors.load(Ordering::Relaxed),
        packets_sent: counters.packets_sent.load(Ordering::Relaxed),
        packets_recv: counters.packets_recv.load(Ordering::Relaxed),
        bytes_sent: counters.bytes_sent.load(Ordering::Relaxed),
        bytes_recv: counters.bytes_recv.load(Ordering::Relaxed),
        compressed_packets_recv: counters.compressed_packets_recv.load(Ordering::Relaxed),
        decompressed_bytes_recv: counters.decompressed_bytes_recv.load(Ordering::Relaxed),
        chunks_recv: counters.chunks_recv.load(Ordering::Relaxed),
        chunk_parse_errors: counters.chunk_parse_errors.load(Ordering::Relaxed),
        system_chat_recv: counters.system_chat_recv.load(Ordering::Relaxed),
        component_parse_errors: counters.component_parse_errors.load(Ordering::Relaxed),
        set_time_recv: counters.set_time_recv.load(Ordering::Relaxed),
        status_ok: true,
        entity_sync_recv: counters.entity_sync_recv.load(Ordering::Relaxed),
        entity_rotation_recv: counters.entity_rotation_recv.load(Ordering::Relaxed),
        head_rotation_recv: counters.head_rotation_recv.load(Ordering::Relaxed),
        block_update_recv: counters.block_update_recv.load(Ordering::Relaxed),
        block_ack_recv: counters.block_ack_recv.load(Ordering::Relaxed),
        inventory_content_recv: counters.inventory_content_recv.load(Ordering::Relaxed),
        container_slot_recv: counters.container_slot_recv.load(Ordering::Relaxed),
        packets_per_sec: (counters.packets_sent.load(Ordering::Relaxed)
            + counters.packets_recv.load(Ordering::Relaxed)) as f64
            / elapsed,
        bytes_per_sec: (counters.bytes_sent.load(Ordering::Relaxed)
            + counters.bytes_recv.load(Ordering::Relaxed)) as f64
            / elapsed,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "players={} workload={} connected={} login_ok={} errors={} packets/s={:.1} bytes/s={:.1}",
            report.players,
            report.workload,
            report.connected,
            report.login_ok,
            report.errors,
            report.packets_per_sec,
            report.bytes_per_sec
        );
    }
    Ok(())
}

fn status_probe(addr: &str) -> Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.write_all(&handshake(addr, 1))?;
    stream.write_all(&packet_raw(0, &[]))?;

    let response = read_status_frame(&mut stream)?;
    let (pid, pid_len) = read_var(&response, 0)?;
    if pid != 0 {
        bail!("status response packet id is {pid:#x}");
    }
    let (json_len, json_len_bytes) = read_var(&response, pid_len)?;
    if json_len < 0 {
        bail!("negative status JSON length");
    }
    let start = pid_len + json_len_bytes;
    let end = start + json_len as usize;
    let json = response
        .get(start..end)
        .ok_or_else(|| anyhow::anyhow!("truncated status JSON"))?;
    if end != response.len() {
        bail!("trailing status response bytes");
    }
    let status: serde_json::Value = serde_json::from_slice(json)?;
    if status["version"]["protocol"].as_i64() != Some(PROTOCOL_VERSION as i64) {
        bail!("status response protocol mismatch");
    }
    if status["players"]["max"].as_u64().unwrap_or(0) == 0
        || status["players"]["online"].as_u64().is_none()
    {
        bail!("status response has invalid player counts");
    }

    let challenge = 0x1020_3040_5060_7080i64;
    let mut ping = Vec::new();
    write_var(&mut ping, 1);
    ping.extend_from_slice(&challenge.to_be_bytes());
    stream.write_all(&frame(&ping))?;
    let pong = read_status_frame(&mut stream)?;
    let (pid, used) = read_var(&pong, 0)?;
    let pong_body = pong
        .get(used..)
        .ok_or_else(|| anyhow::anyhow!("truncated status pong"))?;
    if pid != 1
        || (pong_body != challenge.to_be_bytes().as_slice() && pong_body != 0i64.to_be_bytes())
    {
        bail!("status pong mismatch");
    }
    Ok(())
}

fn read_status_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut rx = Vec::new();
    let mut scratch = [0u8; 1024];
    while Instant::now() < deadline {
        match stream.read(&mut scratch) {
            Ok(0) => bail!("status connection closed"),
            Ok(n) => {
                rx.extend_from_slice(&scratch[..n]);
                if let Some(packet) = read_frame(&mut rx)? {
                    return Ok(packet);
                }
            }
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => return Err(err.into()),
        }
    }
    bail!("status response timed out")
}

fn bot(
    id: usize,
    addr: &str,
    name_prefix: &str,
    workload: Workload,
    stop_at: Instant,
    counters: &Counters,
) -> Result<()> {
    let mut stream = TcpStream::connect(addr)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    counters.connected.fetch_add(1, Ordering::Relaxed);

    send_packet(&mut stream, handshake(addr, 2), counters)?;
    send_packet(
        &mut stream,
        login_start(&format!("{name_prefix}_{id}")),
        counters,
    )?;

    let mut compression = false;
    let mut state = 2;
    let mut rx = Vec::with_capacity(8192);
    let mut scratch = [0u8; 4096];
    let mut play = false;
    let mut step = 0i32;
    let mut next_action = Instant::now();
    let mut origin: Option<(f64, f64, f64)> = None;
    while Instant::now() < stop_at {
        match stream.read(&mut scratch) {
            Ok(0) => bail!("closed"),
            Ok(n) => {
                counters.bytes_recv.fetch_add(n as u64, Ordering::Relaxed);
                rx.extend_from_slice(&scratch[..n]);
                while let Some(packet) = read_frame(&mut rx)? {
                    counters.packets_recv.fetch_add(1, Ordering::Relaxed);
                    let payload = if compression {
                        let (uncompressed, used) = read_var(&packet, 0)?;
                        if uncompressed != 0 {
                            counters
                                .compressed_packets_recv
                                .fetch_add(1, Ordering::Relaxed);
                            let mut decoder = ZlibDecoder::new(&packet[used..]);
                            let mut payload = Vec::with_capacity(uncompressed as usize);
                            decoder.read_to_end(&mut payload)?;
                            counters
                                .decompressed_bytes_recv
                                .fetch_add(payload.len() as u64, Ordering::Relaxed);
                            payload
                        } else {
                            packet[used..].to_vec()
                        }
                    } else {
                        packet
                    };
                    let (pid, pid_len) = read_var(&payload, 0)?;
                    if state == 4 && pid == 0x2d {
                        match validate_chunk_packet(&payload[pid_len..]) {
                            Ok(()) => {
                                counters.chunks_recv.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(err) => {
                                counters.chunk_parse_errors.fetch_add(1, Ordering::Relaxed);
                                bail!("invalid chunk packet: {err}");
                            }
                        }
                    } else if state == 4 && pid == 0x79 {
                        match validate_nbt_text_component(&payload[pid_len..]) {
                            Ok(()) => {
                                counters.system_chat_recv.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(err) => {
                                counters
                                    .component_parse_errors
                                    .fetch_add(1, Ordering::Relaxed);
                                bail!("invalid system chat component: {err}");
                            }
                        }
                    } else if state == 4 && pid == 0x71 {
                        validate_set_time_packet(&payload[pid_len..])
                            .map_err(|err| anyhow::anyhow!("invalid set_time packet: {err}"))?;
                        counters.set_time_recv.fetch_add(1, Ordering::Relaxed);
                    } else if state == 4 && pid == 0x48 && origin.is_none() {
                        origin = Some(parse_player_position(&payload[pid_len..])?);
                    } else if state == 4 {
                        if let Some(counter) = match pid {
                            0x04 => Some(&counters.block_ack_recv),
                            0x08 => Some(&counters.block_update_recv),
                            0x12 => Some(&counters.inventory_content_recv),
                            0x14 => Some(&counters.container_slot_recv),
                            0x23 => Some(&counters.entity_sync_recv),
                            0x38 => Some(&counters.entity_rotation_recv),
                            0x53 => Some(&counters.head_rotation_recv),
                            _ => None,
                        } {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if state == 2 && pid == 3 {
                        compression = true;
                    } else if state == 2 && pid == 2 {
                        send_packet(
                            &mut stream,
                            maybe_compressed(packet_raw(3, &[]), compression),
                            counters,
                        )?;
                        state = 3;
                    } else if state == 3 && pid == 0x0e {
                        send_packet(
                            &mut stream,
                            maybe_compressed(select_known_packs(), compression),
                            counters,
                        )?;
                    } else if state == 3 && pid == 0x03 {
                        send_packet(
                            &mut stream,
                            maybe_compressed(packet_raw(3, &[]), compression),
                            counters,
                        )?;
                        state = 4;
                        play = true;
                        counters.login_ok.fetch_add(1, Ordering::Relaxed);
                    } else if state == 4 && pid == 0x2c {
                        let body = payload;
                        send_packet(
                            &mut stream,
                            maybe_compressed(packet_raw(0x1c, &body[1..]), compression),
                            counters,
                        )?;
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }

        if let Some(origin) = origin.filter(|_| play && Instant::now() >= next_action) {
            send_workload_step(
                &mut stream,
                id,
                workload,
                compression,
                step,
                origin,
                counters,
            )?;
            step += 1;
            let delay = if matches!(workload, Workload::Idle) {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(50)
            };
            next_action += delay;
        }
    }
    Ok(())
}

fn send_workload_step(
    stream: &mut TcpStream,
    id: usize,
    workload: Workload,
    compression: bool,
    step: i32,
    (ox, oy, oz): (f64, f64, f64),
    counters: &Counters,
) -> Result<()> {
    // Block work happens on a small grid beside the spawn column, within reach.
    let cell = (id as i32 * 7 + step / 16) % 21;
    let (bx, by, bz) = (
        ox.floor() as i32 + 1 + cell % 3,
        oy.floor() as i32,
        oz.floor() as i32 - 3 + cell / 3,
    );
    match workload {
        Workload::Idle => {}
        Workload::Walk => {
            let packet = match step % 3 {
                0 => move_pos(ox + step as f64 * 0.1, oy, oz),
                1 => move_pos_rot(ox + step as f64 * 0.1, oy, oz, step as f32, 5.0),
                _ => move_rot(step as f32, -5.0),
            };
            send_packet(stream, maybe_compressed(packet, compression), counters)?;
        }
        Workload::Explore | Workload::ChunkHeavy => send_packet(
            stream,
            maybe_compressed(
                move_pos(ox + (step * 16) as f64, oy, oz + (id % 16) as f64),
                compression,
            ),
            counters,
        )?,
        Workload::BlockHeavy => match step % 4 {
            0 => send_packet(
                stream,
                maybe_compressed(inventory_slot(36, 64, 1), compression),
                counters,
            )?,
            1 => send_packet(
                stream,
                maybe_compressed(block_place(bx, by - 1, bz, step), compression),
                counters,
            )?,
            2 => send_packet(
                stream,
                maybe_compressed(block_break_at(bx, by, bz, step), compression),
                counters,
            )?,
            _ => send_packet(
                stream,
                maybe_compressed(inventory_slot(1, 1, 134), compression),
                counters,
            )?,
        },
        Workload::ChatHeavy => send_packet(
            stream,
            maybe_compressed(chat(&format!("hello_{id}_{step}")), compression),
            counters,
        )?,
        Workload::Mixed => {
            let movement = if step % 2 == 0 {
                move_pos_rot(
                    ox + step as f64 * 0.2,
                    oy,
                    oz + (id % 8) as f64,
                    step as f32,
                    0.0,
                )
            } else {
                move_rot(step as f32, 0.0)
            };
            send_packet(stream, maybe_compressed(movement, compression), counters)?;
            if step % 10 == 0 {
                send_packet(
                    stream,
                    maybe_compressed(chat(&format!("m{id}_{step}")), compression),
                    counters,
                )?;
            }
        }
        Workload::Gameplay => match step % 16 {
            0 => send_packet(
                stream,
                maybe_compressed(inventory_slot(36, 64, 1), compression),
                counters,
            )?,
            1 => send_packet(
                stream,
                maybe_compressed(block_place(bx, by - 1, bz, step), compression),
                counters,
            )?,
            2 => send_packet(
                stream,
                maybe_compressed(inventory_slot(1, 1, 134), compression),
                counters,
            )?,
            4 => send_packet(
                stream,
                maybe_compressed(move_pos(ox + 64.0, oy, oz + (id % 16) as f64), compression),
                counters,
            )?,
            8 => send_packet(
                stream,
                maybe_compressed(move_pos(ox, oy, oz), compression),
                counters,
            )?,
            14 => send_packet(
                stream,
                maybe_compressed(block_break_at(bx, by, bz, step), compression),
                counters,
            )?,
            15 => send_packet(
                stream,
                maybe_compressed(chat(&format!("p{id}_{step}")), compression),
                counters,
            )?,
            _ => send_packet(
                stream,
                maybe_compressed(move_rot(step as f32, 0.0), compression),
                counters,
            )?,
        },
    }
    Ok(())
}

fn send_packet(stream: &mut TcpStream, packet: Vec<u8>, counters: &Counters) -> Result<()> {
    stream.write_all(&packet)?;
    counters.packets_sent.fetch_add(1, Ordering::Relaxed);
    counters
        .bytes_sent
        .fetch_add(packet.len() as u64, Ordering::Relaxed);
    Ok(())
}

fn handshake(addr: &str, next: i32) -> Vec<u8> {
    let host = addr.split(':').next().unwrap_or("127.0.0.1");
    let mut body = Vec::new();
    write_var(&mut body, 0);
    write_var(&mut body, PROTOCOL_VERSION);
    write_string(&mut body, host);
    body.extend_from_slice(&25565i16.to_be_bytes());
    write_var(&mut body, next);
    frame(&body)
}

fn login_start(name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0);
    write_string(&mut body, name);
    frame(&body)
}

fn select_known_packs() -> Vec<u8> {
    packet_raw(7, &[0])
}

fn chat(msg: &str) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x09);
    write_string(&mut body, msg);
    body.extend_from_slice(&0i64.to_be_bytes());
    body.extend_from_slice(&0i64.to_be_bytes());
    body.push(0);
    frame(&body)
}

fn move_pos(x: f64, y: f64, z: f64) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x1e);
    body.extend_from_slice(&x.to_bits().to_be_bytes());
    body.extend_from_slice(&y.to_bits().to_be_bytes());
    body.extend_from_slice(&z.to_bits().to_be_bytes());
    body.push(1);
    frame(&body)
}

fn move_pos_rot(x: f64, y: f64, z: f64, yaw: f32, pitch: f32) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x1f);
    body.extend_from_slice(&x.to_bits().to_be_bytes());
    body.extend_from_slice(&y.to_bits().to_be_bytes());
    body.extend_from_slice(&z.to_bits().to_be_bytes());
    body.extend_from_slice(&yaw.to_bits().to_be_bytes());
    body.extend_from_slice(&pitch.to_bits().to_be_bytes());
    body.push(1);
    frame(&body)
}

fn move_rot(yaw: f32, pitch: f32) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x20);
    body.extend_from_slice(&yaw.to_bits().to_be_bytes());
    body.extend_from_slice(&pitch.to_bits().to_be_bytes());
    body.push(1);
    frame(&body)
}

fn inventory_slot(slot: i16, count: i32, item_id: i32) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x12);
    write_var(&mut body, 0);
    write_var(&mut body, 0);
    body.extend_from_slice(&slot.to_be_bytes());
    body.push(0);
    write_var(&mut body, 0);
    write_var(&mut body, 1);
    body.extend_from_slice(&slot.to_be_bytes());
    body.push(1);
    write_var(&mut body, item_id);
    write_var(&mut body, count);
    body.push(0);
    body.push(0);
    frame(&body)
}

/// Right-clicks the top face of the block at (x, y, z).
fn block_place(x: i32, y: i32, z: i32, step: i32) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x42);
    write_var(&mut body, 0);
    write_position(&mut body, x, y, z);
    write_var(&mut body, 1);
    body.extend_from_slice(&0.5f32.to_bits().to_be_bytes());
    body.extend_from_slice(&1.0f32.to_bits().to_be_bytes());
    body.extend_from_slice(&0.5f32.to_bits().to_be_bytes());
    body.push(0);
    body.push(0);
    write_var(&mut body, step);
    frame(&body)
}

fn block_break_at(x: i32, y: i32, z: i32, step: i32) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, 0x29);
    write_var(&mut body, 2);
    write_position(&mut body, x, y, z);
    body.push(1);
    write_var(&mut body, step);
    frame(&body)
}

fn packet_raw(id: i32, rest: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    write_var(&mut body, id);
    body.extend_from_slice(rest);
    frame(&body)
}

fn maybe_compressed(packet: Vec<u8>, compression: bool) -> Vec<u8> {
    if !compression {
        return packet;
    }
    let mut payload = Vec::new();
    payload.push(0);
    payload.extend_from_slice(&packet[var_len(packet.len() as i32)..]);
    frame(&payload)
}

fn read_frame(buf: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let Ok((len, used)) = read_var(buf, 0) else {
        return Ok(None);
    };
    let len = len as usize;
    if buf.len() < used + len {
        return Ok(None);
    }
    let packet = buf[used..used + len].to_vec();
    buf.drain(..used + len);
    Ok(Some(packet))
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    write_var(&mut out, payload.len() as i32);
    out.extend_from_slice(payload);
    out
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    write_var(out, s.len() as i32);
    out.extend_from_slice(s.as_bytes());
}

fn write_position(out: &mut Vec<u8>, x: i32, y: i32, z: i32) {
    let packed =
        ((x as i64 & 0x3ffffff) << 38) | ((z as i64 & 0x3ffffff) << 12) | (y as i64 & 0xfff);
    out.extend_from_slice(&packed.to_be_bytes());
}

fn write_var(out: &mut Vec<u8>, mut value: i32) {
    loop {
        if (value & !0x7f) == 0 {
            out.push(value as u8);
            return;
        }
        out.push(((value & 0x7f) | 0x80) as u8);
        value = ((value as u32) >> 7) as i32;
    }
}

fn read_var(data: &[u8], mut pos: usize) -> Result<(i32, usize)> {
    let start = pos;
    let mut num_read = 0;
    let mut result = 0i32;
    loop {
        if pos >= data.len() {
            bail!("varint underflow");
        }
        let byte = data[pos];
        pos += 1;
        result |= ((byte & 0x7f) as i32) << (7 * num_read);
        num_read += 1;
        if num_read > 5 {
            bail!("varint too large");
        }
        if byte & 0x80 == 0 {
            return Ok((result, pos - start));
        }
    }
}

struct SliceCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> SliceCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.data.len() {
            bail!("buffer underflow");
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn i16(&mut self) -> Result<i16> {
        let bytes = self.take(2)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn i32(&mut self) -> Result<i32> {
        let bytes = self.take(4)?;
        Ok(i32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        let bytes = self.take(8)?;
        Ok(i64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn var_i32(&mut self) -> Result<i32> {
        let (value, used) = read_var(self.data, self.pos)?;
        self.pos += used;
        Ok(value)
    }
}

fn validate_nbt_text_component(data: &[u8]) -> Result<()> {
    let mut c = SliceCursor::new(data);
    let root = c.u8()?;
    if root != 0x0a {
        bail!("root tag is {root:#04x}, expected compound");
    }
    let tag = c.u8()?;
    if tag != 0x08 {
        bail!("first compound tag is {tag:#04x}, expected string");
    }
    let name_len = c.i16()?;
    if name_len != 4 {
        bail!("text tag name length {name_len}");
    }
    if c.take(4)? != b"text" {
        bail!("text tag name mismatch");
    }
    let text_len = c.i16()?;
    if text_len < 0 {
        bail!("negative text length");
    }
    c.take(text_len as usize)?;
    let end = c.u8()?;
    if end != 0 {
        bail!("compound end tag is {end:#04x}");
    }
    c.u8()?;
    if c.remaining() != 0 {
        bail!("trailing bytes after system chat");
    }
    Ok(())
}

fn validate_set_time_packet(data: &[u8]) -> Result<()> {
    let mut c = SliceCursor::new(data);
    c.i64()?;
    c.u8()?;
    if c.remaining() != 0 {
        bail!("trailing set_time bytes");
    }
    Ok(())
}

fn validate_chunk_packet(data: &[u8]) -> Result<()> {
    let mut c = SliceCursor::new(data);
    let _chunk_x = c.i32()?;
    let _chunk_z = c.i32()?;
    let heightmaps = c.var_i32()?;
    if heightmaps != 0 {
        bail!("unsupported heightmap count {heightmaps}");
    }
    let chunk_len = c.var_i32()?;
    if chunk_len < 0 {
        bail!("negative chunk data length");
    }
    validate_chunk_data(c.take(chunk_len as usize)?)?;
    let block_entities = c.var_i32()?;
    if block_entities != 0 {
        bail!("unsupported block entity count {block_entities}");
    }
    for _ in 0..4 {
        let longs = c.var_i32()?;
        if !(0..=16).contains(&longs) {
            bail!("invalid light mask length {longs}");
        }
        for _ in 0..longs {
            c.i64()?;
        }
    }
    let sky_arrays = c.var_i32()?;
    if !(0..=26).contains(&sky_arrays) {
        bail!("invalid sky light array count {sky_arrays}");
    }
    for _ in 0..sky_arrays {
        let len = c.var_i32()?;
        if !(0..=2048).contains(&len) {
            bail!("invalid sky light array length {len}");
        }
        c.take(len as usize)?;
    }
    let block_arrays = c.var_i32()?;
    if !(0..=26).contains(&block_arrays) {
        bail!("invalid block light array count {block_arrays}");
    }
    for _ in 0..block_arrays {
        let len = c.var_i32()?;
        if !(0..=2048).contains(&len) {
            bail!("invalid block light array length {len}");
        }
        c.take(len as usize)?;
    }
    if c.remaining() != 0 {
        bail!("trailing chunk packet bytes");
    }
    Ok(())
}

fn validate_chunk_data(data: &[u8]) -> Result<()> {
    let mut c = SliceCursor::new(data);
    for section in 0..24 {
        let _block_count = c.i16()?;
        let _fluid_count = c.i16()?;
        validate_palette_container(&mut c, 4096, 4..=8)
            .map_err(|err| anyhow::anyhow!("section {section} block states: {err}"))?;
        validate_palette_container(&mut c, 64, 1..=3)
            .map_err(|err| anyhow::anyhow!("section {section} biomes: {err}"))?;
    }
    if c.remaining() != 0 {
        bail!("trailing chunk-data bytes");
    }
    Ok(())
}

fn validate_palette_container(
    c: &mut SliceCursor<'_>,
    entries: usize,
    indirect_bits: std::ops::RangeInclusive<u8>,
) -> Result<()> {
    let bits = c.u8()?;
    if bits == 0 {
        c.var_i32()?;
        return Ok(());
    }
    if !indirect_bits.contains(&bits) {
        bail!("unsupported bits per entry {bits}");
    }
    let palette_len = c.var_i32()?;
    if palette_len < 1 || palette_len > 1 << bits {
        bail!("invalid palette length {palette_len} for {bits} bits");
    }
    for _ in 0..palette_len {
        c.var_i32()?;
    }
    let per_long = 64 / bits as usize;
    for _ in 0..entries.div_ceil(per_long) {
        c.i64()?;
    }
    Ok(())
}

fn parse_player_position(data: &[u8]) -> Result<(f64, f64, f64)> {
    let mut c = SliceCursor::new(data);
    let _teleport_id = c.var_i32()?;
    let x = f64::from_bits(c.i64()? as u64);
    let y = f64::from_bits(c.i64()? as u64);
    let z = f64::from_bits(c.i64()? as u64);
    if !(x.is_finite() && y.is_finite() && z.is_finite()) {
        bail!("non-finite spawn position");
    }
    Ok((x, y, z))
}

fn var_len(mut value: i32) -> usize {
    let mut len = 1;
    while (value & !0x7f) != 0 {
        len += 1;
        value = ((value as u32) >> 7) as i32;
    }
    len
}
