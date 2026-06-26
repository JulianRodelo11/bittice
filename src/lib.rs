pub mod core {
    pub mod data_paths;
    pub mod fd_limits;
    pub mod auth;
    pub mod config;
    pub mod date_utils;
    pub mod join_query;
    pub mod writer;
    pub mod query;
    pub mod saved_queries;
    pub mod storage;
    pub mod types;
    pub mod expression;
    pub mod cdc;
    pub mod cdc_durability;
    pub mod migrate_primary_index;
    pub mod migrate_exact_index;
    pub mod update;
    pub mod uninstall;
    pub mod credentials;
    pub mod control_plane;
    pub mod control_plane_gate;
    pub mod mirror_consistency;
    pub mod mirror_maintenance;
    pub mod warm_config;
}

pub mod commands {}

pub mod cli;

pub mod repl;

pub mod server;

