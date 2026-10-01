/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright The Asahi Linux Contributors
 */

pub mod cd321x;
pub mod i2c;
#[cfg(target_os = "linux")]
pub mod kernel;
#[cfg(target_os = "linux")]
pub mod sysfs;

#[cfg(target_os = "linux")]
use crate::sysfs::{
    connector_label, get_dev_from_typec_port, get_i2c_dev_from_typec_port, get_i2c_sysfs_device,
    get_typec_port_from_connector, has_i2c_cd321x, recover_debugusb_enumeration,
    recover_missing_partner, recover_vdm_timeout, wait_for_debugusb,
};
use env_logger::Env;
use log::{error, info, warn};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::{fs, process::ExitCode};

#[derive(Debug)]
#[allow(dead_code)]
enum Error {
    Device,
    DeviceNotFound,
    Compatible,
    FeatureMissing,
    TypecController,
    InvalidArgument,
    ReconnectTimeout,
    ControllerTimeout,
    VdmReplyTimeout,
    VdmRejected,
    DebugUsbEnumerationTimeout,
    I2C,
    Io(std::io::Error),
    Utf8(std::str::Utf8Error),
    Parse(std::num::ParseIntError),
}

type Result<T> = std::result::Result<T, Error>;

/// How the host's USB-C controller is reached.
enum Target {
    /// Raw register access on /dev/i2c-N (CD321x on M1/M2).
    I2c { bus: String, addr: u16 },
    /// The kernel tipd driver's debugfs interface, by device name (e.g. "0-0c"); for
    /// controllers userspace cannot open directly, such as SN201202x on SPMI.
    #[cfg(target_os = "linux")]
    Kernel { name: String },
}

impl Target {
    fn open(&self) -> Result<Box<dyn cd321x::BusDevice>> {
        match self {
            Target::I2c { bus, addr } => Ok(Box::new(i2c::I2CBusDevice::open(bus, *addr)?)),
            #[cfg(target_os = "linux")]
            Target::Kernel { name } => Ok(Box::new(kernel::KernelBusDevice::open(name)?)),
        }
    }

    /// The controller's sysfs device, for Type-C/USB recovery and DebugUSB checks.
    #[cfg(target_os = "linux")]
    fn sysfs_device(&self) -> Option<PathBuf> {
        match self {
            Target::I2c { bus, addr } => get_i2c_sysfs_device(bus, *addr),
            Target::Kernel { name } => kernel::sysfs_device(name),
        }
    }

    /// Rebinding the HPM driver puts an SPMI HPM to sleep and wakes it again.  Never do that
    /// to a controller in DBMa through the kernel interface: on hosts whose boot loader keeps
    /// Apple Debug USB armed (DBMa from boot), it could take that mode down.
    #[cfg(target_os = "linux")]
    fn may_reprobe(&self) -> bool {
        let Target::Kernel { name } = self else {
            return true;
        };
        let mut mode = [0u8; 4];
        let in_dbma = kernel::KernelBusDevice::open(name)
            .and_then(|mut dev| cd321x::BusDevice::read_block(&mut dev, 0x03, &mut mode))
            .map_or(true, |_| &mode == b"DBMa");
        if in_dbma {
            warn!("Not rebinding {name}: it is in DBMa (or unreadable), which a rebind could drop");
        }
        !in_dbma
    }
}

#[cfg(target_os = "linux")]
fn recover(target: &Target, recovery: fn(&std::path::Path) -> Result<()>) -> Result<()> {
    match target.sysfs_device() {
        Some(device) if target.may_reprobe() => recovery(&device),
        _ => Ok(()),
    }
}

/// Pick the kernel tipd interface when the controller is not reachable as an i2c CD321x.
#[cfg(target_os = "linux")]
fn kernel_target(name: String) -> Result<Target> {
    if !kernel::has_interface(&name) {
        error!(
            "{}/{name}/command not found: the kernel's tipd driver must be bound to {name} \
             (debugfs mounted, kernel with the tipd debugfs interface)",
            kernel::TIPD_DEBUGFS_ROOT
        );
        return Err(Error::DeviceNotFound);
    }
    Ok(Target::Kernel { name })
}

#[cfg(target_os = "linux")]
fn target_from_connector(connector: &str) -> Result<Target> {
    let port = get_typec_port_from_connector(connector)?;
    if let Some((bus, addr)) = get_i2c_dev_from_typec_port(&port) {
        return Ok(Target::I2c { bus, addr });
    }
    let device = get_dev_from_typec_port(&port).ok_or(Error::DeviceNotFound)?;
    let name = device
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::DeviceNotFound)?;
    kernel_target(name.to_string())
}

/// No connector and no explicit --bus/--address: keep the historical i2c default where an
/// i2c CD321x exists, otherwise use the kernel interface if exactly one controller has it.
#[cfg(target_os = "linux")]
fn default_kernel_target() -> Result<Option<Target>> {
    if has_i2c_cd321x() {
        return Ok(None);
    }
    let names = kernel::list_devices();
    match names.as_slice() {
        [name] => kernel_target(name.clone()).map(Some),
        [] => {
            error!(
                "No CD321x on i2c and no controller under {}; not guessing an i2c bus",
                kernel::TIPD_DEBUGFS_ROOT
            );
            Err(Error::DeviceNotFound)
        }
        _ => {
            error!("Several USB-C controllers found; choose one with --connector:");
            for name in &names {
                let label = kernel::sysfs_device(name)
                    .and_then(|device| connector_label(&device))
                    .unwrap_or_else(|| "no label".to_string());
                error!("  {name}: {label}");
            }
            Err(Error::DeviceNotFound)
        }
    }
}

fn execute_command(
    matches: &clap::ArgMatches,
    target: &Target,
    code: &str,
    reset_controller: bool,
) -> Result<()> {
    let bus_dev = target.open()?;
    let mut device = if reset_controller {
        cd321x::Device::new_for_reset(bus_dev, code.to_string())
    } else {
        cd321x::Device::new(bus_dev, code.to_string())?
    };

    match matches.subcommand() {
        Some(("dfu", _)) => {
            device.dfu()?;
        }
        Some(("reboot", args)) => match args.subcommand() {
            Some(("serial", _)) => {
                device.reboot_wait()?;
                device.serial()?;
            }
            Some(("debugusb", _)) => {
                device.reboot_wait()?;
                device.debugusb()?;
            }
            None => {
                device.reboot()?;
            }
            _ => {}
        },
        Some(("nop", _)) => {}
        Some(("serial", _)) => {
            device.serial()?;
        }
        Some(("debugusb", _)) => {
            device.debugusb()?;
        }
        Some(("disconnect", _)) => {
            device.disconnect()?;
        }
        Some(("reset-controller", _)) => {
            device.reset_controller_nowait()?;
        }
        _ => {}
    }
    Ok(())
}

fn execute_debugusb(target: &Target, code: &str) -> Result<()> {
    let bus_dev = target.open()?;
    let mut device = cd321x::Device::new(bus_dev, code.to_string())?;
    device.debugusb()
}

fn is_debugusb_command(matches: &clap::ArgMatches) -> bool {
    match matches.subcommand() {
        Some(("debugusb", _)) | Some(("reboot debugusb", _)) => true,
        Some(("reboot", args)) => args.subcommand_name() == Some("debugusb"),
        _ => false,
    }
}

fn is_reboot_debugusb_command(matches: &clap::ArgMatches) -> bool {
    match matches.subcommand() {
        Some(("reboot debugusb", _)) => true,
        Some(("reboot", args)) => args.subcommand_name() == Some("debugusb"),
        _ => false,
    }
}

fn vdmtool() -> Result<()> {
    let matches = clap::command!()
        .arg(
            clap::arg!(-b --bus [BUS] "i2c bus of the USB-C controller device.")
                .default_value("/dev/i2c-0"),
        )
        .arg(
            clap::arg!(-a --address [ADDRESS] "i2c target address of the USB-C controller device.")
                .default_value("0x38"),
        )
        .arg(clap::arg!(-c --connector [CONNECTOR] "(Partial) connector label of the USB-C controller device."))
        .subcommand(
            clap::Command::new("reboot")
                .about("reboot the target")
                .subcommand(
                    clap::Command::new("serial").about("reboot the target and enter serial mode"),
                )
                .subcommand(
                    clap::Command::new("debugusb")
                        .about("reboot the target and enter Debug USB mode"),
                ),
        )
        // dummy commands to display help for "reboot serial/debugusb"
        .subcommand(
            clap::Command::new("reboot serial").about("reboot the target and enter serial mode"),
        )
        .subcommand(
            clap::Command::new("reboot debugusb")
                .about("reboot the target and enter Debug USB mode"),
        )
        .subcommand(clap::Command::new("serial").about("enter serial mode on both ends"))
        .subcommand(clap::Command::new("debugusb").about("enter Debug USB mode on target"))
        .subcommand(clap::Command::new("dfu").about("put the target into DFU mode"))
        .subcommand(clap::Command::new("disconnect").about("Simulate USB unplug/plug"))
        .subcommand(
            clap::Command::new("reset-controller")
                .about("Soft-reset only the selected host USB-C PD controller"),
        )
        .subcommand(clap::Command::new("nop").about("Do nothing"))
        .arg_required_else_help(true)
        .get_matches();

    let compat: Vec<u8> = fs::read("/proc/device-tree/compatible").map_err(Error::Io)?;
    let compat = std::str::from_utf8(&compat[0..10]).map_err(Error::Utf8)?;
    let (manufacturer, device) = compat.split_once(",").ok_or(Error::Compatible)?;
    if manufacturer != "apple" {
        error!("Host is not an Apple silicon system: \"{compat}\"");
        return Err(Error::Compatible);
    }

    let i2c_target = || -> Result<Target> {
        let addr_str = matches.get_one::<String>("address").unwrap();
        let addr = if let Some(stripped) = addr_str.strip_prefix("0x") {
            u16::from_str_radix(stripped, 16).map_err(Error::Parse)?
        } else {
            addr_str.parse::<u16>().map_err(Error::Parse)?
        };
        let bus = matches.get_one::<String>("bus").unwrap().to_string();
        Ok(Target::I2c { bus, addr })
    };
    let explicit_i2c = ["bus", "address"]
        .iter()
        .any(|arg| matches.value_source(arg) == Some(clap::ValueSource::CommandLine));

    #[cfg(target_os = "linux")]
    let target = if let Some(connector) = matches.get_one::<String>("connector") {
        target_from_connector(&connector.to_ascii_lowercase())?
    } else if explicit_i2c {
        i2c_target()?
    } else {
        match default_kernel_target()? {
            Some(target) => target,
            None => i2c_target()?,
        }
    };
    #[cfg(not(target_os = "linux"))]
    let target = {
        let _ = explicit_i2c;
        if matches.contains_id("connector") {
            return Err(Error::DeviceNotFound);
        }
        i2c_target()?
    };

    match &target {
        Target::I2c { bus, addr } => info!("Using I2C bus:{bus} address:{addr:#x}"),
        #[cfg(target_os = "linux")]
        Target::Kernel { name } => info!(
            "Using the kernel tipd interface {}/{name}",
            kernel::TIPD_DEBUGFS_ROOT
        ),
    }

    let code = device.to_uppercase();
    let reset_controller = matches.subcommand_name() == Some("reset-controller");
    let nop = matches.subcommand_name() == Some("nop");
    let debugusb = is_debugusb_command(&matches);
    let reboot_debugusb = is_reboot_debugusb_command(&matches);
    #[cfg(target_os = "linux")]
    if !reset_controller && !nop {
        recover(&target, recover_missing_partner)?;
    }

    let mut result = execute_command(&matches, &target, &code, reset_controller);
    if matches!(&result, Err(Error::VdmReplyTimeout)) && !reset_controller && !nop {
        #[cfg(target_os = "linux")]
        {
            recover(&target, recover_vdm_timeout)?;
            result = if reboot_debugusb {
                warn!("The target reboot was already attempted; retrying only the DebugUSB arm");
                execute_debugusb(&target, &code)
            } else {
                warn!("Retrying command once after HPM recovery");
                execute_command(&matches, &target, &code, false)
            };
        }
    }
    result?;

    #[cfg(target_os = "linux")]
    if debugusb {
        let hpm = target.sysfs_device().unwrap_or_default();
        if let Some(device) = wait_for_debugusb(&hpm)? {
            info!("DebugUSB enumerated at {}", device.display());
            return Ok(());
        }

        warn!("DebugUSB VDM was acknowledged, but no USB device enumerated");
        recover(&target, recover_debugusb_enumeration)?;
        warn!("Retrying only the DebugUSB arm after exact-port recovery");
        execute_debugusb(&target, &code)?;

        if let Some(device) = wait_for_debugusb(&hpm)? {
            info!("DebugUSB enumerated at {} after recovery", device.display());
            return Ok(());
        }

        error!("DebugUSB never enumerated after an acknowledged VDM and one scoped recovery");
        return Err(Error::DebugUsbEnumerationTimeout);
    }

    Ok(())
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    match vdmtool() {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            error!("vdmtool: {:?}", e);
            ExitCode::FAILURE
        }
    }
}
