#![allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    missing_docs,
    clippy::all,
    clippy::unwrap_used,
    clippy::expect_used,
    unsafe_code
)]
pub mod __interfaces {
    use wayland_client::protocol::__interfaces::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
    wayland_scanner::generate_interfaces!("./protocols/hyprland-toplevel-export-v1.xml");
}
use self::__interfaces::*;
use wayland_client;
use wayland_client::protocol::*;
use wayland_protocols_wlr::foreign_toplevel::v1::client::*;
wayland_scanner::generate_client_code!("./protocols/hyprland-toplevel-export-v1.xml");
