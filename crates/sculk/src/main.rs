use sculk::server::{Config, Server};
use sculk::Result;
use std::env;
use std::io;
use std::path::PathBuf;

const HELP: &str = "\
Sculk: a tiny, fast Minecraft Java Edition server

Usage: sculk [OPTIONS]

Options:
      --host <HOST>            Bind address [default: 0.0.0.0]
      --port <PORT>            Bind port [default: 25565]
      --max-players <N>        Player limit [default: 1000]
      --seed <SEED>            World seed (number or text) [default: 0]
      --world <FILE>           Save block edits and inventories to FILE
      --motd <TEXT>            Server list description [default: A Sculk server]
      --no-compression         Disable protocol compression
  -h, --help                   Print help
  -V, --version                Print version";

fn main() -> Result<()> {
    let mut host = "0.0.0.0".to_owned();
    let mut port: u16 = 25565;
    let mut config = Config {
        bind: "0.0.0.0:25565".parse()?,
        max_players: 1000,
        compression: true,
        seed: 0,
        world_path: None,
        motd: "A Sculk server".to_owned(),
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        let mut value = || -> Result<String> {
            inline
                .clone()
                .or_else(|| args.next())
                .ok_or_else(|| usage(format!("missing value for {flag}")))
        };
        match flag.as_str() {
            "--host" => host = value()?,
            "--port" => {
                port = value()?
                    .parse()
                    .map_err(|e| usage(format!("--port: {e}")))?
            }
            "--max-players" => {
                config.max_players = value()?
                    .parse()
                    .map_err(|e| usage(format!("--max-players: {e}")))?
            }
            "--seed" => config.seed = parse_seed(&value()?),
            "--world" => config.world_path = Some(PathBuf::from(value()?)),
            "--motd" => config.motd = value()?,
            "--no-compression" => config.compression = false,
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("sculk {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            _ => return Err(usage(format!("unknown argument {arg}; see --help"))),
        }
    }
    config.bind = format!("{host}:{port}")
        .parse()
        .or_else(|_| format!("[{host}]:{port}").parse())
        .map_err(|e| usage(format!("--host: {e}")))?;
    Server::bind(config)?.run()
}

/// Numeric seeds are used as-is; any other text is hashed (FNV-1a).
fn parse_seed(text: &str) -> u64 {
    text.parse::<i64>().map(|n| n as u64).unwrap_or_else(|_| {
        text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    })
}

fn usage(message: String) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}
