pub mod core {
    pub mod auth;
    pub mod config;
    pub mod date_utils;
    pub mod writer;
    pub mod query;
    pub mod saved_queries;
    pub mod storage;
    pub mod types;
    pub mod expression;
    pub mod cdc;
    pub mod update;
    pub mod uninstall;
    pub mod vpn;
}

pub mod commands {}

pub mod cli;

pub mod repl;

pub mod server;

