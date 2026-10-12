//! CTX.app: the native macOS window for the `ctx` server. It holds no shells and no scrollback of
//! its own; it shows what the server has and sends it your keys.

#![allow(clippy::type_complexity)]

pub mod app;
pub mod boxes;
pub mod client;
pub mod config;
pub mod headless;
pub mod keys;
pub mod layout;
pub mod links;
pub mod model;
pub mod render;
pub mod selection;
pub mod shortcuts;
pub mod state;
pub mod text;
pub mod theme;
