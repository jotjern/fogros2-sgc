#[macro_use]
extern crate log;

pub mod network;
pub mod structs;

pub mod connection_store;
pub mod db;
pub mod pipeline;
pub mod commands;
pub mod topic_manager;
pub mod routing;

use utils::error::Result;

pub fn start() -> Result<()> {
    Ok(())
}
