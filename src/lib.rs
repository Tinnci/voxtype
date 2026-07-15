//! Linux desktop integration and daemon orchestration for `VoxType`.

pub mod audio;
pub mod client;
pub mod config;
pub mod daemon;
pub mod desktop;
pub mod fcitx;

pub const DBUS_NAME: &str = "io.github.tinnci.VoxType";
pub const DBUS_PATH: &str = "/io/github/tinnci/VoxType1";
pub const DBUS_INTERFACE: &str = "io.github.tinnci.VoxType1";
