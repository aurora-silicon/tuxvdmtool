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

fn read_be_u32(path: &Path) -> Option<u32> {
    let bytes = fs::read(path).ok()?;
    Some(u32::from_be_bytes(bytes.as_slice().try_into().ok()?))
}

fn find_node_by_phandle(root: &Path, phandle: u32) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        if read_be_u32(&path.join("phandle")) == Some(phandle) {
            return Some(path);
        }
        if let Some(found) = find_node_by_phandle(&path, phandle) {
            return Some(found);
        }
    }
    None
}

fn usb_controller_for_hpm_in(
    device: &Path,
    device_tree: &Path,
    platform_devices: &Path,
) -> Option<PathBuf> {
    // The USB 2.0 endpoint is port@0 of a "ports" node, or the only "port" node of a
    // connector that has nothing else (e.g. J700's "USB-C Left-front").
    let connector = device.join("of_node/connector");
    let remote_phandle = ["ports/port@0", "port"]
        .iter()
        .find_map(|port| read_be_u32(&connector.join(port).join("endpoint/remote-endpoint")))?;
    let remote_node = find_node_by_phandle(device_tree, remote_phandle)?;
    let usb_node = remote_node.ancestors().find(|ancestor| {
        ancestor
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("usb@"))
    })?;
    let address = usb_node.file_name()?.to_str()?.strip_prefix("usb@")?;
    fs::canonicalize(platform_devices.join(format!("{address}.usb"))).ok()
}

fn usb_controller_for_hpm(device: &Path) -> Option<PathBuf> {
    usb_controller_for_hpm_in(
        device,
        Path::new("/sys/firmware/devicetree/base"),
        Path::new("/sys/bus/platform/devices"),
    )
}

fn debugusb_device_in(root: &Path, controller: Option<&Path>) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(root).map_err(Error::Io)? {
        let path = entry.map_err(Error::Io)?.path();
        if read_trimmed(&path.join("idVendor")).as_deref() != Some("05ac")
            || read_trimmed(&path.join("idProduct")).as_deref() != Some("1881")
        {
            continue;
        }
        if let Some(controller) = controller {
            let Ok(device_path) = fs::canonicalize(&path) else {
                continue;
            };
            if !device_path.starts_with(controller) {
                continue;
            }
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
pub(crate) fn wait_for_debugusb(device: &Path) -> Result<Option<PathBuf>> {
    let controller = usb_controller_for_hpm(device);
    if let Some(controller) = &controller {
        debug!(
            "Verifying DebugUSB below selected controller {}",
            controller.display()
        );
    } else {
        warn!("Could not map the selected HPM to a USB controller; using global DebugUSB scan");
    }

    let start = Instant::now();
    while start.elapsed() < DEBUGUSB_WAIT {
        if let Some(device) =
            debugusb_device_in(Path::new("/sys/bus/usb/devices"), controller.as_deref())?
        {
            return Ok(Some(device));
        }
        thread::sleep(DEBUGUSB_POLL);
    }
    Ok(None)
}

/// The controller device a Type-C port belongs to, e.g.
/// /sys/devices/platform/soc/.../spmi-0/0-0c for an SN201202x on SPMI.
pub(crate) fn get_dev_from_typec_port(typec_path: &Path) -> Option<PathBuf> {
    fs::canonicalize(typec_path.join("device")).ok()
}

/// Whether any i2c device is an Apple CD321x, i.e. a controller the raw i2c
/// transport (and its /dev/i2c-0 0x38 default) can be meant for.
pub(crate) fn has_i2c_cd321x() -> bool {
    has_i2c_cd321x_in(Path::new("/sys/bus/i2c/devices"))
}

fn has_i2c_cd321x_in(i2c_devices: &Path) -> bool {
    let Ok(entries) = fs::read_dir(i2c_devices) else {
        return false;
    };
    entries.filter_map(|entry| entry.ok()).any(|entry| {
        fs::read(entry.path().join("of_node/compatible"))
            .is_ok_and(|compat| compat.split(|&b| b == 0).any(|c| c == b"apple,cd321x"))
    })
}

/// The connector label of an HPM device, if its DT node has one.
pub(crate) fn connector_label(device: &Path) -> Option<String> {
    let label = fs::read(device.join("of_node/connector/label")).ok()?;
    let label = label.split(|&b| b == 0).next()?;
    Some(String::from_utf8_lossy(label).into_owned())
}

pub(crate) fn get_i2c_dev_from_typec_port(typec_path: &Path) -> Option<(String, u16)> {
    let path = get_dev_from_typec_port(typec_path)?;

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

pub(crate) fn get_i2c_sysfs_device(bus: &str, addr: u16) -> Option<PathBuf> {
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

/// Kernel drivers for the HPMs this tool talks to: tps6598x (i2c CD321x) and
/// sn201202x (SPMI, M3 and later).  Both are frontends of the tipd core.
fn is_hpm_driver(name: &str) -> bool {
    matches!(name, "tps6598x" | "sn201202x")
}

/// Recover a stale Linux Type-C view before touching an HPM.
///
/// With raw I2C, tuxvdmtool has to force-open an HPM that is owned by tps6598x.
/// If Linux misses a target reconnect while the HPM is in DBMa mode, the
/// physical PD contract can exist while the kernel has no Type-C partner.
/// Rebinding only the selected HPM makes the tipd driver reread the live
/// contract; it does not reset or unbind the host USB controller.
fn reprobe_hpm(device: &Path) -> Result<()> {
    if !device.exists() {
        return Ok(());
    }

    let device_id = device
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::DeviceNotFound)?;
    let Ok(driver) = fs::canonicalize(device.join("driver")) else {
        warn!("Selected HPM {device_id} has no driver bound; skipping Type-C recovery");
        return Ok(());
    };
    if !driver
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_hpm_driver)
    {
        warn!(
            "Selected HPM {device_id} is not bound to tps6598x/sn201202x; skipping Type-C recovery"
        );
        return Ok(());
    }

    warn!("Reprobeing only HPM {device_id}");
    fs::write(driver.join("unbind"), device_id).map_err(Error::Io)?;
    thread::sleep(Duration::from_millis(250));
    fs::write(driver.join("bind"), device_id).map_err(Error::Io)?;

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if has_typec_partner(device) {
            info!("Type-C partner recovered on HPM {device_id}");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    warn!("HPM {device_id} reprobed, but no Type-C partner appeared");
    Ok(())
}

pub(crate) fn recover_missing_partner(device: &Path) -> Result<()> {
    if !device.exists() || has_typec_partner(device) {
        return Ok(());
    }

    warn!("Type-C partner is missing in Linux; recovering the selected HPM");
    reprobe_hpm(device)
}

pub(crate) fn recover_vdm_timeout(device: &Path) -> Result<()> {
    warn!("VDM received no reply; resynchronizing the selected HPM before one retry");
    reprobe_hpm(device)
}

pub(crate) fn recover_debugusb_enumeration(device: &Path) -> Result<()> {
    warn!("Recovering only the selected Type-C path before the DebugUSB retry");
    reprobe_hpm(device)
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

        let found = debugusb_device_in(&root, None).unwrap();
        assert_eq!(found.as_deref(), Some(debugusb.as_path()));

        fs::write(debugusb.join("idProduct"), "1880\n").unwrap();
        assert_eq!(debugusb_device_in(&root, None).unwrap(), None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maps_an_hpm_usb2_endpoint_to_its_platform_controller() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tuxvdmtool-dt-{nonce}"));
        let device = root.join("i2c/0-003f");
        let remote = device.join("of_node/connector/ports/port@0/endpoint");
        let endpoint = root.join("dt/soc/usb@b02280000/ports/port@0/endpoint");
        let platform = root.join("platform");
        let controller = platform.join("b02280000.usb");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&endpoint).unwrap();
        fs::create_dir_all(&controller).unwrap();
        fs::write(remote.join("remote-endpoint"), 0x59_u32.to_be_bytes()).unwrap();
        fs::write(endpoint.join("phandle"), 0x59_u32.to_be_bytes()).unwrap();

        assert_eq!(
            usb_controller_for_hpm_in(&device, &root.join("dt"), &platform),
            Some(fs::canonicalize(&controller).unwrap())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maps_a_single_port_connector_to_its_platform_controller() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tuxvdmtool-dt1-{nonce}"));
        let device = root.join("spmi/0-0a");
        let remote = device.join("of_node/connector/port/endpoint");
        let endpoint =
            root.join("dt/soc/usb@40a280000/usb2-role-inputs/usb2-role-input@1/port/endpoint");
        let platform = root.join("platform");
        let controller = platform.join("40a280000.usb");
        fs::create_dir_all(&remote).unwrap();
        fs::create_dir_all(&endpoint).unwrap();
        fs::create_dir_all(&controller).unwrap();
        fs::write(remote.join("remote-endpoint"), 0x2a_u32.to_be_bytes()).unwrap();
        fs::write(endpoint.join("phandle"), 0x2a_u32.to_be_bytes()).unwrap();
        fs::write(
            device.join("of_node/connector/label"),
            b"USB-C Left-front\0",
        )
        .unwrap();

        assert_eq!(
            usb_controller_for_hpm_in(&device, &root.join("dt"), &platform),
            Some(fs::canonicalize(&controller).unwrap())
        );
        assert_eq!(
            connector_label(&device).as_deref(),
            Some("USB-C Left-front")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn detects_cd321x_only_by_compatible() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tuxvdmtool-i2c-{nonce}"));
        // J700: /dev/i2c-0 carries the cs42l83 codec, not an HPM
        let codec = root.join("0-0048/of_node");
        fs::create_dir_all(&codec).unwrap();
        fs::write(codec.join("compatible"), b"cirrus,cs42l83\0").unwrap();
        assert!(!has_i2c_cd321x_in(&root));

        let hpm = root.join("1-0038/of_node");
        fs::create_dir_all(&hpm).unwrap();
        fs::write(hpm.join("compatible"), b"apple,cd321x\0").unwrap();
        assert!(has_i2c_cd321x_in(&root));
        assert!(!has_i2c_cd321x_in(&root.join("missing")));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maps_typec_ports_on_spmi_to_no_i2c_device() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("tuxvdmtool-typec-{nonce}"));
        let spmi_dev = root.join("devices/spmi-0/0-0c");
        let i2c_dev = root.join("devices/i2c-1/1-0038");
        let spmi_port = root.join("class/port0");
        let i2c_port = root.join("class/port1");
        for dir in [&spmi_dev, &i2c_dev, &spmi_port, &i2c_port] {
            fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(&spmi_dev, spmi_port.join("device")).unwrap();
        std::os::unix::fs::symlink(&i2c_dev, i2c_port.join("device")).unwrap();

        assert_eq!(get_i2c_dev_from_typec_port(&spmi_port), None);
        assert_eq!(
            get_dev_from_typec_port(&spmi_port),
            Some(fs::canonicalize(&spmi_dev).unwrap())
        );
        assert_eq!(
            get_i2c_dev_from_typec_port(&i2c_port),
            Some(("/dev/i2c-1".to_string(), 0x38))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn accepts_both_tipd_frontends_for_reprobe() {
        assert!(is_hpm_driver("tps6598x"));
        assert!(is_hpm_driver("sn201202x"));
        assert!(!is_hpm_driver("cs42l83"));
    }
}
