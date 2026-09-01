//! Basalt: an embedded SQL database engine built from scratch.
pub mod btree;
pub mod cli;
pub mod crc;
pub mod database;
pub mod db;
pub mod engine;
pub mod eval;
pub mod planner;
pub mod sql;
pub mod storage;
pub mod types;
pub mod wal;

pub use database::{Connection, Database, Transaction};
