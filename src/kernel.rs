/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright The Asahi Linux Contributors
 *
 * Controller access through the kernel tipd driver's debugfs interface,
 * /sys/kernel/debug/usb/tipd/<device>/{command,regs}.  This reaches
 * controllers that are not on an i2c bus userspace can open (SN201202x on
 * SPMI, M3 and later) and runs every command under the driver's lock instead
 * of racing it on the bus.
 *
 *   command  write: 4CC + DATA1 bytes, executed whole by the driver
 *            (LOCK, DBMa, VDMs, DVEn, DISC only); read: resulting DATA1
 *   regs     pread() with the register as offset, at most 64 bytes
 */

use crate::cd321x::BusDevice;
use crate::{Error, Result};
use log::{debug, error};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const TIPD_DEBUGFS_ROOT: &str = "/sys/kernel/debug/usb/tipd";
const MAX_BLOCK: usize = 64;
/* Same budget the register-level path gives a busy CMD1 */
const CMD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
/*
 * The driver answers EBUSY at once while CMD1 still holds a command, so writing the request
 * again is the CMD1 poll; done this way it runs under the driver's lock, which a read of CMD1
 * through regs followed by a write would not.  Same interval as the register-level poll.
 */
const CMD_IDLE_POLL: Duration = Duration::from_millis(10);

pub(crate) struct KernelBusDevice {
    name: String,
    command: File,
    regs: File,
}

impl KernelBusDevice {
    pub(crate) fn open(name: &str) -> Result<KernelBusDevice> {
        Self::open_in(Path::new(TIPD_DEBUGFS_ROOT), name)
    }

    fn open_in(root: &Path, name: &str) -> Result<KernelBusDevice> {
        let dir = root.join(name);
        let command = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dir.join("command"))
            .map_err(Error::Io)?;
        let regs = File::open(dir.join("regs")).map_err(Error::Io)?;
        Ok(KernelBusDevice {
            name: name.to_string(),
            command,
            regs,
        })
    }
}

/// Whether the kernel offers the tipd debugfs interface for `name` (a device name such as `0-0c`).
pub(crate) fn has_interface(name: &str) -> bool {
    has_interface_in(Path::new(TIPD_DEBUGFS_ROOT), name)
}

fn has_interface_in(root: &Path, name: &str) -> bool {
    root.join(name).join("command").exists()
}

/// All controllers with a tipd debugfs interface, sorted by device name.
pub(crate) fn list_devices() -> Vec<String> {
    list_devices_in(Path::new(TIPD_DEBUGFS_ROOT))
}

fn list_devices_in(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join("command").exists())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect();
    names.sort();
    names
}

/// The sysfs device behind a tipd debugfs directory.  The directory is named after the
/// device (dev_name()), which is unique within its bus; SPMI is checked first because
/// that is the bus this interface exists for.
pub(crate) fn sysfs_device(name: &str) -> Option<PathBuf> {
    sysfs_device_in(Path::new("/sys/bus"), name)
}

fn sysfs_device_in(sys_bus: &Path, name: &str) -> Option<PathBuf> {
    ["spmi", "i2c"]
        .iter()
        .map(|bus| sys_bus.join(bus).join("devices").join(name))
        .find(|path| path.exists())
}

fn map_errno(err: std::io::Error) -> Error {
    match err.raw_os_error() {
        Some(libc_errno::ETIMEDOUT) => Error::ControllerTimeout,
        Some(libc_errno::EINVAL) | Some(libc_errno::EPERM) => Error::TypecController,
        Some(libc_errno::EOPNOTSUPP) => Error::FeatureMissing,
        _ => Error::Io(err),
    }
}

/* errno values of Linux on all architectures this tool runs on */
mod libc_errno {
    pub(super) const EPERM: i32 = 1;
    pub(super) const EBUSY: i32 = 16;
    pub(super) const EINVAL: i32 = 22;
    pub(super) const ETIMEDOUT: i32 = 110;
    pub(super) const EOPNOTSUPP: i32 = 95;
}

impl BusDevice for KernelBusDevice {
    fn write_block(&mut self, reg: u8, _data: &[u8]) -> Result<()> {
        error!(
            "Raw register writes (register {reg:#04x}) are not available through the kernel tipd interface"
        );
        Err(Error::FeatureMissing)
    }

    fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()> {
        if buf.is_empty() || buf.len() > MAX_BLOCK {
            return Err(Error::InvalidArgument);
        }
        self.regs.read_exact_at(buf, reg.into()).map_err(Error::Io)
    }

    fn exec_cmd(&mut self, cmd: &[u8; 4], data: &[u8]) -> Option<Result<()>> {
        Some(self.exec(cmd, data))
    }
}

impl KernelBusDevice {
    fn exec(&mut self, cmd: &[u8; 4], data: &[u8]) -> Result<()> {
        if data.len() > MAX_BLOCK {
            return Err(Error::InvalidArgument);
        }
        let request = [cmd.as_slice(), data].concat();
        let start = Instant::now();
        loop {
            /* One write() is one command: the driver rejects partial requests. */
            match self.command.write(&request) {
                Ok(n) if n == request.len() => break,
                Ok(n) => {
                    error!("Short write of {n}/{} bytes to tipd command", request.len());
                    return Err(Error::Io(ErrorKind::WriteZero.into()));
                }
                Err(err) if err.raw_os_error() == Some(libc_errno::EBUSY) => {
                    /* CMD1 still busy, e.g. the controller finishing its own VDM traffic */
                    if start.elapsed() > CMD_IDLE_TIMEOUT {
                        debug!("{}: CMD1 stayed busy", self.name);
                        return Err(Error::ControllerTimeout);
                    }
                    thread::sleep(CMD_IDLE_POLL);
                }
                Err(err) => {
                    debug!(
                        "{}: {} failed: {err}",
                        self.name,
                        String::from_utf8_lossy(cmd)
                    );
                    return Err(map_errno(err));
                }
            }
        }

        let mut result = [0u8; 1];
        if self.command.read_exact_at(&mut result, 0).is_ok() {
            debug!(
                "{}: {} result {:#04x}",
                self.name,
                String::from_utf8_lossy(cmd),
                result[0]
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tuxvdmtool-{tag}-{nonce}"))
    }

    #[test]
    fn lists_only_directories_with_a_command_file() {
        let root = scratch("tipd");
        for name in ["1-0a", "0-0c"] {
            fs::create_dir_all(root.join(name)).unwrap();
            fs::write(root.join(name).join("command"), b"").unwrap();
        }
        fs::create_dir_all(root.join("stale")).unwrap();

        assert_eq!(list_devices_in(&root), vec!["0-0c", "1-0a"]);
        assert!(has_interface_in(&root, "0-0c"));
        assert!(!has_interface_in(&root, "stale"));
        assert!(list_devices_in(&root.join("missing")).is_empty());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finds_the_spmi_device_before_i2c() {
        let root = scratch("bus");
        fs::create_dir_all(root.join("spmi/devices/0-0c")).unwrap();
        fs::create_dir_all(root.join("i2c/devices/1-0038")).unwrap();

        assert_eq!(
            sysfs_device_in(&root, "0-0c"),
            Some(root.join("spmi/devices/0-0c"))
        );
        assert_eq!(
            sysfs_device_in(&root, "1-0038"),
            Some(root.join("i2c/devices/1-0038"))
        );
        assert_eq!(sysfs_device_in(&root, "2-0a"), None);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_registers_at_their_offset_and_refuses_writes() {
        let root = scratch("regs");
        let dir = root.join("0-0c");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("command"), b"").unwrap();
        /* A plain file stands in for regs: offset 3 is MODE */
        fs::write(dir.join("regs"), b"\0\0\0DBMa").unwrap();

        let mut dev = KernelBusDevice::open_in(&root, "0-0c").unwrap();
        let mut mode = [0u8; 4];
        dev.read_block(0x03, &mut mode).unwrap();
        assert_eq!(&mode, b"DBMa");
        assert!(matches!(
            dev.read_block(0x03, &mut [0u8; 65]),
            Err(Error::InvalidArgument)
        ));
        assert!(matches!(
            dev.write_block(0x08, b"Gaid"),
            Err(Error::FeatureMissing)
        ));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sends_a_command_as_one_write() {
        let root = scratch("cmd");
        let dir = root.join("0-0c");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("command"), b"").unwrap();
        fs::write(dir.join("regs"), b"").unwrap();

        let mut dev = KernelBusDevice::open_in(&root, "0-0c").unwrap();
        assert!(matches!(dev.exec_cmd(b"DBMa", &[1]), Some(Ok(()))));
        assert_eq!(fs::read(dir.join("command")).unwrap(), b"DBMa\x01");

        fs::remove_dir_all(root).unwrap();
    }
}
