pub mod allocation_metrics;
pub mod chunk;
mod deflate;
mod inventory;
mod items;
pub mod metrics;
pub mod protocol;
mod recipes;
mod save;
pub mod server;
#[rustfmt::skip]
pub mod vanilla;
#[rustfmt::skip]
mod vanilla_items;
pub mod world;

use std::error::Error;

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
