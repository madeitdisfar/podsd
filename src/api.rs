//! Names of podsd's D-Bus API, shared by the daemon and podsctl.
//!
//! Session bus, service `io.github.madeitdisfar.Podsd`:
//!   /io/github/madeitdisfar/Podsd              org.freedesktop.DBus.ObjectManager,
//!                                              io.github.madeitdisfar.Podsd.Manager1
//!   /io/github/madeitdisfar/Podsd/dev_XX_..._XX io.github.madeitdisfar.Podsd.Device1
//! The full interface is described in README.md.

pub const BUS_NAME: &str = "io.github.madeitdisfar.Podsd";
pub const ROOT_PATH: &str = "/io/github/madeitdisfar/Podsd";
pub const DEVICE_IFACE: &str = "io.github.madeitdisfar.Podsd.Device1";
#[allow(dead_code)]
pub const MANAGER_IFACE: &str = "io.github.madeitdisfar.Podsd.Manager1";

#[allow(dead_code)]
pub fn device_path(address: &str) -> String {
    format!("{}/dev_{}", ROOT_PATH, address.replace(':', "_"))
}
