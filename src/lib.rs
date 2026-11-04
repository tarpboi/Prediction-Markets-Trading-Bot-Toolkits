//! Polymarket Toolkits — shared library.
//!
//! Multi-venue prediction-market trading engine. Ten strategies share one
//! engine and risk layer (see [`bot`]). They default to dry run, and no live order
//! has been sent through them yet.

pub mod bot;
pub mod config;
pub mod models;
pub mod service;
pub mod ui;
pub mod utils;
