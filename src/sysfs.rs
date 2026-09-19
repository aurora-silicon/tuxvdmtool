/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright The Asahi Linux Contributors
 */

use log::{debug, info, warn};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, Result};

const DEBUGUSB_WAIT: Duration = Duration::from_secs(8);
const DEBUGUSB_POLL: Duration = Duration::from_millis(100);

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

fn debugusb_device_in(root: &Path) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(root).map_err(Error::Io)? {
        let path = entry.map_err(Error::Io)?.path();
        if read_trimmed(&path.join("idVendor")).as_deref() != Some("05ac")
            || read_trimmed(&path.join("idProduct")).as_deref() != Some("1881")
        {
            continue;
        }
        return Ok(Some(path));
    }
    Ok(None)
}

/// Wait for the KIS/DebugUSB function that kisd can actually open.
///
/// Do not try to identify a new enumeration by bus path or `devnum`: when the
/// Apple DWC3 host is rebuilt for the Type-C reconnect, Linux legitimately
/// reuses both values.  The command has already completed by the time this is
/// called, so the exact KIS VID:PID being present is the useful contract.
pub(crate) fn wait_for_debugusb() -> Result<Option<PathBuf>> {
    let start = Instant::now();
    while start.elapsed() < DEBUGUSB_WAIT {
        if let Some(device) = debugusb_device_in(Path::new("/sys/bus/usb/devices"))? {
            return Ok(Some(device));
        }
        thread::sleep(DEBUGUSB_POLL);
    }
    Ok(None)
}

pub(crate) fn get_i2c_dev_from_typec_port(typec_path: &Path) -> Option<(String, u16)> {
    let path = std::fs::canonicalize(typec_path.join("device")).ok()?;

    // First, check that this device is located on an i2c bus
    let bus_id = path
        .parent()?
        .file_name()?
        .to_str()
        .unwrap()
        .strip_prefix("i2c-")?;

    // Only consider I2C devices with the pattern  ("%d-%04x", bus, addr)
    let (bus, addr) = path.file_name()?.to_str()?.split_once("-")?;

    if bus != bus_id {
        return None;
    };

    let addr = u16::from_str_radix(addr, 16).unwrap();
    Some((format!("/dev/i2c-{}", bus), addr))
}

pub(crate) fn get_typec_port_from_connector(connector: &str) -> Result<PathBuf> {
    let mut match_len = usize::MAX;
    let mut candidate: Option<PathBuf> = None;

    // iterate over all typec ports
    for entry in fs::read_dir("/sys/class/typec/").map_err(Error::Io)? {
        let path = entry.map_err(Error::Io)?.path();
        if let Some(port) = path.file_name() {
            // look only into /sys/class/typec/port[0-9]
            if let Some(port_id) = port.to_str().unwrap().strip_prefix("port") {
                if !port_id.chars().all(char::is_numeric) {
                    continue;
                }
                let label_path = path.join("device/of_node/connector/label");
                let Ok(label) = fs::read_to_string(label_path) else {
                    continue;
                };

                // convert to lower case for case insensitive match
                let label = label.to_ascii_lowercase();

                if label.starts_with("usb-c ") && label.contains(connector) {
                    debug!("Found connector with label '{label}' at {path:?}");
                    // Use the device with the shortest match so that cases like
                    // "USB-C Back Left" and "USB-C Back Left Middle" can be matched
                    // consistently. "back left" will always match the former.
                    if label.len() < match_len {
                        candidate = Some(path);
                        match_len = label.len();
                    }
                }
            }
        }
    }

    candidate.ok_or(Error::DeviceNotFound)
}

fn get_i2c_sysfs_device(bus: &str, addr: u16) -> Option<PathBuf> {
    let bus = Path::new(bus).file_name()?.to_str()?.strip_prefix("i2c-")?;
    Some(PathBuf::from(format!(
        "/sys/bus/i2c/devices/{bus}-{addr:04x}"
    )))
}

fn has_typec_partner(device: &Path) -> bool {
    let Ok(ports) = fs::read_dir(device.join("typec")) else {
        return false;
    };

    ports.filter_map(|entry| entry.ok()).any(|port| {
        let Ok(children) = fs::read_dir(port.path()) else {
            return false;
        };
        children.filter_map(|entry| entry.ok()).any(|child| {
            child
                .file_name()
                .to_str()
                .is_some_and(|name| name.ends_with("-partner"))
        })
    })
}

/// Recover a stale Linux Type-C view before touching an HPM through raw I2C.
///
/// tuxvdmtool has to force-open an HPM that is owned by tps6598x. If Linux
/// misses a target reconnect while the HPM is in DBMa mode, the physical PD
/// contract can exist while the kernel has no Type-C partner. Rebinding only
/// the selected HPM makes tps6598x reread the live contract; it does not reset
/// or unbind the host USB controller.
fn reprobe_hpm(bus: &str, addr: u16) -> Result<()> {
    let Some(device) = get_i2c_sysfs_device(bus, addr) else {
        return Ok(());
    };
    if !device.exists() {
        return Ok(());
    }

    let device_id = device
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::DeviceNotFound)?;
    let driver = fs::canonicalize(device.join("driver")).map_err(Error::Io)?;
    if driver.file_name().and_then(|name| name.to_str()) != Some("tps6598x") {
        warn!("Selected I2C device has no tps6598x driver; skipping Type-C recovery");
        return Ok(());
    }

    warn!("Reprobeing only HPM {device_id}");
    fs::write(driver.join("unbind"), device_id).map_err(Error::Io)?;
    thread::sleep(Duration::from_millis(250));
    fs::write(driver.join("bind"), device_id).map_err(Error::Io)?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if has_typec_partner(&device) {
            info!("Type-C partner recovered on HPM {device_id}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    warn!("HPM {device_id} reprobed, but no Type-C partner appeared");
    Ok(())
}

pub(crate) fn recover_missing_partner(bus: &str, addr: u16) -> Result<()> {
    let Some(device) = get_i2c_sysfs_device(bus, addr) else {
        return Ok(());
    };
    if !device.exists() || has_typec_partner(&device) {
        return Ok(());
    }

    warn!("Type-C partner is missing in Linux; recovering the selected HPM");
    reprobe_hpm(bus, addr)
}

pub(crate) fn recover_vdm_timeout(bus: &str, addr: u16) -> Result<()> {
    warn!("VDM received no reply; resynchronizing the selected HPM before one retry");
    reprobe_hpm(bus, addr)
}

pub(crate) fn recover_debugusb_enumeration(bus: &str, addr: u16) -> Result<()> {
    warn!("Recovering only the selected Type-C path before the DebugUSB retry");
    reprobe_hpm(bus, addr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn finds_only_the_debugusb_vid_pid() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tuxvdmtool-usb-{nonce}"));
        let debugusb = root.join("1-1");
        let other = root.join("1-2");
        fs::create_dir_all(&debugusb).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::write(debugusb.join("idVendor"), "05ac\n").unwrap();
        fs::write(debugusb.join("idProduct"), "1881\n").unwrap();
        fs::write(debugusb.join("devnum"), "2\n").unwrap();
        fs::write(other.join("idVendor"), "05ac\n").unwrap();
        fs::write(other.join("idProduct"), "1234\n").unwrap();
        fs::write(other.join("devnum"), "3\n").unwrap();

        let found = debugusb_device_in(&root).unwrap();
        assert_eq!(found.as_deref(), Some(debugusb.as_path()));

        fs::write(debugusb.join("idProduct"), "1880\n").unwrap();
        assert_eq!(debugusb_device_in(&root).unwrap(), None);

        fs::remove_dir_all(root).unwrap();
    }
}
