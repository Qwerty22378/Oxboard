//! Types shared by the engine, its host process, the controller and the UI: the graph
//! description, engine events, the engine pipe protocol (and the controller's end of it), and
//! the UI protocol. No platform code.

pub mod engine_client;
pub mod events;
pub mod graph;
pub mod protocol;
pub mod ui;
