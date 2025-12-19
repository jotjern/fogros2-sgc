#[macro_use]
extern crate log;

pub mod network;
pub mod structs;

pub mod commands;
pub mod connection_store;
pub mod db;
pub mod pipeline;
pub mod routing;
pub mod topic_manager;

use utils::error::Result;

pub fn start() -> Result<()> {
    Ok(())
}
