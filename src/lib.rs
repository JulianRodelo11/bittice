pub mod core {
    pub mod config;
    pub mod date_utils;
    pub mod schema;
    pub mod writer;
    pub mod query;
    pub mod saved_queries;
    pub mod storage;
}

pub mod commands {
    pub mod load;
    pub mod search;
}

pub mod cli;

pub mod repl;

pub mod ui;

pub mod server;
