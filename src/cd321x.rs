/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Copyright The Asahi Linux Contributors
 */

use crate::{Error, Result};
use log::{error, info};
use std::{
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

pub(crate) trait BusDevice {
    fn write_block(&mut self, reg: u8, data: &[u8]) -> Result<()>;
    fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()>;

    /// Run a whole 4CC command (DATA1 + CMD1 + completion) if the transport does that
    /// itself, as the kernel tipd driver does.  `None` means the transport only moves
    /// register contents and `Device` drives CMD1 through `read_block`/`write_block`.
    fn exec_cmd(&mut self, _cmd_tag: &[u8; 4], _in_data: &[u8]) -> Option<Result<()>> {
        None
    }
}

/*
 * Reboot handshake timing, the values of macvdmtool's WaitConnected().  Neither wait has a
 * signal this tool can read instead: the tipd driver owns the controller's interrupt and
 * clears INT_EVENT, so no latched plug event is left to read, and POWER_STATUS cannot tell
 * the old contract from the new one until the old one has dropped.  After the reconnect the
 * target's acknowledgement of the next VDM is the only sign that it is ready for one.
 */
/// Time the target gets to drop its contract after acknowledging the reboot VDM.
const REBOOT_DROP_WAIT: Duration = Duration::from_secs(1);
/// How long the connection bit is polled for the target's new contract.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Interval between reads of the connection bit.
const RECONNECT_POLL: Duration = Duration::from_millis(100);
/// Time the target gets after its new contract before the next VDM is sent.
const CONTRACT_SETTLE_WAIT: Duration = Duration::from_secs(1);

/// Command budget of the kernel tipd driver's tps6598x_exec_cmd().
const CMD_TIMEOUT: Duration = Duration::from_secs(1);
/// VDMs budget, as in the kernel tipd debugfs command table.
const VDMS_TIMEOUT: Duration = Duration::from_millis(200);
/// How long CMD1 may stay busy with the controller's own work before a command is written.
const CMD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);
/// Interval between reads of CMD1 while it is busy before a command is written.
const CMD_IDLE_POLL: Duration = Duration::from_millis(10);
/// MODE follows a completed DBMa command within this time (the kernel's cd321x_wait_dbma()).
const MODE_SWITCH_TIMEOUT: Duration = Duration::from_millis(100);
/// Interval between reads of MODE while it switches (the kernel's cd321x_wait_dbma()).
const MODE_SWITCH_POLL: Duration = Duration::from_millis(1);

const TPS_REG_MODE: u8 = 0x03;
const TPS_REG_CMD1: u8 = 0x08;
const TPS_REG_DATA1: u8 = 0x09;
const TPS_REG_POWER_STATUS: u8 = 0x3f;
const TPS_REG_VDM_RX_STATUS: u8 = 0x4d;

const VDM_REPLY_TIMEOUT: Duration = Duration::from_secs(2);
/// Interval between reads of the VDM receive status while waiting for the reply.
const VDM_REPLY_POLL: Duration = Duration::from_millis(10);

#[allow(dead_code)]
enum VdmSopType {
    Sop = 0b00,
    SopPrime = 0b01,
    SopPrimePrime = 0b10,
    SopStar = 0b11,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq)]
enum TpsMode {
    App,
    Boot,
    Bist,
    Disc,
    Ptch,
    Dbma,
}

impl FromStr for TpsMode {
    type Err = ();
    fn from_str(input: &str) -> std::result::Result<TpsMode, ()> {
        match input {
            "APP " => Ok(TpsMode::App),
            "BOOT" => Ok(TpsMode::Boot),
            "BIST" => Ok(TpsMode::Bist),
            "DISC" => Ok(TpsMode::Disc),
            "PTCH" => Ok(TpsMode::Ptch),
            "DBMa" => Ok(TpsMode::Dbma),
            _ => Err(()),
        }
    }
}

fn is_invalid_cmd(val: u32) -> bool {
    val == 0x444d4321
}

pub(crate) struct Device {
    bus_dev: Box<dyn BusDevice>,
    key: Vec<u8>,
    skip_shutdown: bool,
    /// This session unlocked the controller and entered DBMa, so it has to leave
    /// it again.  A controller found in DBMa (e.g. a boot loader keeping Apple
    /// Debug USB armed on it) is left exactly as it was.
    entered_dbma: bool,
}

impl Device {
    pub(crate) fn new_for_reset(bus_dev: Box<dyn BusDevice>, code: String) -> Self {
        Self {
            bus_dev,
            key: code.into_bytes().into_iter().rev().collect::<Vec<u8>>(),
            skip_shutdown: true,
            entered_dbma: false,
        }
    }

    pub(crate) fn new(bus_dev: Box<dyn BusDevice>, code: String) -> Result<Self> {
        let mut device = Self {
            bus_dev,
            key: code.into_bytes().into_iter().rev().collect::<Vec<u8>>(),
            skip_shutdown: false,
            entered_dbma: false,
        };
        match device.get_mode()? {
            TpsMode::App => {
                device.lock(device.key.clone().as_slice())?;
                device.entered_dbma = true;
                device.dbma(true)?;
            }
            TpsMode::Dbma => {
                info!("Controller already in DBMa mode; using it and leaving it in DBMa");
            }
            _ => return Err(Error::TypecController),
        }

        Ok(device)
    }

    fn exec_cmd(&mut self, cmd_tag: &[u8; 4], in_data: &[u8]) -> Result<()> {
        self.exec_cmd_tmo(cmd_tag, in_data, CMD_TIMEOUT)
    }

    fn exec_cmd_tmo(
        &mut self,
        cmd_tag: &[u8; 4],
        in_data: &[u8],
        cmd_timeout: Duration,
    ) -> Result<()> {
        // A transport that executes commands itself owns CMD1 and its timeouts.
        if let Some(result) = self.bus_dev.exec_cmd(cmd_tag, in_data) {
            return result;
        }

        // First: wait for CMD1 to become idle.  During a reconnect the PD
        // controller can still be finishing its own VDM traffic; treating
        // that normal transient as a hard error loses the early boot window.
        // "!CMD" is what an earlier unrecognized command left behind: the
        // controller is idle (as the kernel tipd driver treats it too).
        let idle_start = Instant::now();
        loop {
            let mut status_buf = [0u8; 4];
            self.bus_dev.read_block(TPS_REG_CMD1, &mut status_buf)?;
            let val = u32::from_le_bytes(status_buf);
            if val == 0 || is_invalid_cmd(val) {
                break;
            }
            if idle_start.elapsed() > CMD_IDLE_TIMEOUT {
                info!("Busy Check Timed Out with VAL = {val:#x}");
                return Err(Error::ControllerTimeout);
            }
            thread::sleep(CMD_IDLE_POLL);
        }

        // Write input Data to DATA1
        if !in_data.is_empty() {
            self.bus_dev.write_block(TPS_REG_DATA1, in_data)?;
        }

        // Write 4-byte command tag
        self.bus_dev.write_block(TPS_REG_CMD1, cmd_tag)?;

        // Poll until CMD1 becomes zero or timeout
        let start = Instant::now();
        loop {
            let mut status_buf = [0u8; 4];
            self.bus_dev.read_block(TPS_REG_CMD1, &mut status_buf)?;
            let val = u32::from_le_bytes(status_buf);
            if is_invalid_cmd(val) {
                info!("Invalid Command");
                return Err(Error::TypecController);
            }
            if val == 0 {
                break;
            }
            if start.elapsed() > cmd_timeout {
                return Err(Error::ControllerTimeout);
            }
        }
        Ok(())
    }

    fn get_mode(&mut self) -> Result<TpsMode> {
        let mut buf = [0u8; 4];
        self.bus_dev.read_block(TPS_REG_MODE, &mut buf)?;
        let s = std::str::from_utf8(&buf).unwrap();
        let m = TpsMode::from_str(s).map_err(|_| Error::TypecController)?;
        Ok(m)
    }

    fn lock(&mut self, key: &[u8]) -> Result<()> {
        self.exec_cmd(b"LOCK", key)
    }

    fn dbma(&mut self, debug: bool) -> Result<()> {
        let data: [u8; 1] = if debug { [1] } else { [0] };
        self.exec_cmd(b"DBMa", &data)?;
        // The command completes before MODE has switched; the firmware
        // reports the new mode once it is in it.
        let expected = if debug { TpsMode::Dbma } else { TpsMode::App };
        let start = Instant::now();
        loop {
            if self.get_mode()? == expected {
                return Ok(());
            }
            if start.elapsed() > MODE_SWITCH_TIMEOUT {
                return Err(Error::TypecController);
            }
            thread::sleep(MODE_SWITCH_POLL);
        }
    }

    fn vdms(&mut self, sop: VdmSopType, vdos: &[u32]) -> Result<()> {
        if vdos.is_empty() || vdos.len() > 7 {
            return Err(Error::InvalidArgument);
        }
        if self.get_mode()? != TpsMode::Dbma {
            return Err(Error::TypecController);
        }

        /*
         * Match macvdmtool's AppleHPMLib transaction exactly.  CMD1 becoming
         * idle only means that the local CD321x accepted the VDMs command; it
         * does not mean that the partner received or acknowledged the VDM.
         * Record the receive-status generation before transmission, then wait
         * for a new reply and validate its structured-VDM header before DBMa
         * is torn down by Drop.
         */
        let mut reply = [0u8; 64];
        self.bus_dev.read_block(TPS_REG_VDM_RX_STATUS, &mut reply)?;
        let rx_status = reply[0];
        let data = [
            vec![((sop as u8) << 4) | vdos.len() as u8],
            vdos.iter().flat_map(|val| val.to_le_bytes()).collect(),
        ]
        .concat();
        self.exec_cmd_tmo(b"VDMs", &data, VDMS_TIMEOUT)?;

        let start = Instant::now();
        loop {
            self.bus_dev.read_block(TPS_REG_VDM_RX_STATUS, &mut reply)?;
            if reply[0] != rx_status {
                break;
            }
            if start.elapsed() > VDM_REPLY_TIMEOUT {
                error!("Target did not reply to VDM (RX status remained {rx_status:#04x})");
                return Err(Error::VdmReplyTimeout);
            }
            thread::sleep(VDM_REPLY_POLL);
        }

        let reply_header = u32::from_le_bytes(reply[1..5].try_into().unwrap());
        let expected_header = vdos[0] | 0x40;
        if reply_header != expected_header {
            error!(
                "Target rejected VDM: expected reply header {expected_header:#010x}, got {reply_header:#010x}"
            );
            return Err(Error::VdmRejected);
        }

        info!("Target acknowledged VDM (reply header {reply_header:#010x})");
        Ok(())
    }

    fn dven(&mut self, vdos: &[u32]) -> Result<()> {
        let data: Vec<u8> = vdos.iter().flat_map(|val| val.to_le_bytes()).collect();
        self.exec_cmd(b"DVEn", &data)
    }

    pub(crate) fn check_connected(&mut self) -> Result<bool> {
        let mut buf = [0u8; 2];
        self.bus_dev.read_block(TPS_REG_POWER_STATUS, &mut buf)?;
        let power_status = u16::from_le_bytes(buf);
        Ok((power_status & 1) != 0)
    }

    pub(crate) fn dfu(&mut self) -> Result<()> {
        let vdos: [u32; 3] = [0x5ac8012, 0x106, 0x80010000];
        info!("Rebooting target into DFU mode...");
        self.vdms(VdmSopType::SopStar, &vdos)
    }

    pub(crate) fn reboot(&mut self) -> Result<()> {
        let vdos: [u32; 3] = [0x5ac8012, 0x105, 0x80000000];
        info!("Rebooting target into normal mode...");
        self.vdms(VdmSopType::SopStar, &vdos)
    }

    pub(crate) fn reboot_wait(&mut self) -> Result<()> {
        self.reboot()?;
        info!("Waiting for target connection...");

        /* Match macvdmtool: allow the target one second to reboot, then poll
         * for the fresh contract.  Requiring an observed disconnect races a
         * fast reconnect and misses the early serial/DebugUSB VDM window. */
        thread::sleep(REBOOT_DROP_WAIT);
        let reconnect_start = Instant::now();
        loop {
            if self.check_connected().unwrap_or(false) {
                break;
            }
            thread::sleep(RECONNECT_POLL);
            if reconnect_start.elapsed() > RECONNECT_TIMEOUT {
                error!("Target did not reconnect after reboot");
                return Err(Error::ReconnectTimeout);
            }
        }
        thread::sleep(CONTRACT_SETTLE_WAIT);
        info!("Target reconnected");
        Ok(())
    }

    /// Send a SOP* VDM in the window after a (re)connect.  The controller is
    /// then still running its own VDM traffic and a VDMs can miss its budget;
    /// it is sent again until RECONNECT_TIMEOUT has passed.  Each attempt
    /// starts by waiting for CMD1 to go idle, so no delay goes between them.
    fn vdms_after_connect(&mut self, vdos: &[u32]) -> Result<()> {
        let start = Instant::now();
        loop {
            match self.vdms(VdmSopType::SopStar, vdos) {
                Err(Error::ControllerTimeout) if start.elapsed() < RECONNECT_TIMEOUT => {
                    info!("VDMs did not complete in time; sending it again");
                }
                result => return result,
            }
        }
    }

    pub(crate) fn serial(&mut self) -> Result<()> {
        let vdos: [u32; 2] = [0x5ac8012, 0x1840306];
        info!("Putting target into serial mode...");
        self.vdms_after_connect(&vdos)?;
        info!("Putting local end into serial mode... ");
        if self.get_mode()? != TpsMode::Dbma {
            return Err(Error::TypecController);
        }
        self.dven(&vdos[1..2])
    }

    pub(crate) fn debugusb(&mut self) -> Result<()> {
        let vdos: [u32; 2] = [0x5ac8012, 0x1824606];
        info!("Putting target into DebugUSB mode...");
        self.vdms_after_connect(&vdos)
    }

    pub(crate) fn disconnect(&mut self) -> Result<()> {
        let data: [u8; 1] = [3];
        self.exec_cmd(b"DISC", &data)?;
        Ok(())
    }

    pub(crate) fn reset_controller_nowait(&mut self) -> Result<()> {
        /*
         * Gaid resets the HPM itself.  Submit it directly in APP mode without
         * polling or running the normal Drop teardown: the I2C endpoint is
         * intentionally unavailable while firmware restarts.
         */
        self.bus_dev.write_block(TPS_REG_CMD1, b"Gaid")?;
        self.skip_shutdown = true;
        Ok(())
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.skip_shutdown || !self.entered_dbma {
            return;
        }
        let lock: [u8; 4] = [0, 0, 0, 0];
        let _ = self.dbma(false);
        let _ = self.lock(&lock);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Mode = Rc<RefCell<[u8; 4]>>;
    type CommandLog = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

    /// A command-level transport (like the kernel tipd interface) that records commands
    /// and models MODE for LOCK/DBMa.
    struct MockBus {
        mode: Mode,
        log: CommandLog,
    }

    impl BusDevice for MockBus {
        fn write_block(&mut self, _reg: u8, _data: &[u8]) -> Result<()> {
            Err(Error::FeatureMissing)
        }

        fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()> {
            buf.fill(0);
            if reg == TPS_REG_MODE {
                buf.copy_from_slice(&*self.mode.borrow());
            }
            Ok(())
        }

        fn exec_cmd(&mut self, cmd: &[u8; 4], data: &[u8]) -> Option<Result<()>> {
            if cmd == b"DBMa" {
                *self.mode.borrow_mut() = if data[0] != 0 { *b"DBMa" } else { *b"APP " };
            }
            self.log
                .borrow_mut()
                .push((String::from_utf8_lossy(cmd).into_owned(), data.to_vec()));
            Some(Ok(()))
        }
    }

    fn session(mode: &[u8; 4]) -> (Mode, CommandLog) {
        let mode = Rc::new(RefCell::new(*mode));
        let log = Rc::new(RefCell::new(Vec::new()));
        let bus = MockBus {
            mode: mode.clone(),
            log: log.clone(),
        };
        let device = Device::new(Box::new(bus), "J700".to_string()).unwrap();
        drop(device);
        (mode, log)
    }

    #[test]
    fn enters_and_leaves_dbma_from_app_mode() {
        let (mode, log) = session(b"APP ");
        assert_eq!(
            *log.borrow(),
            vec![
                ("LOCK".to_string(), b"007J".to_vec()),
                ("DBMa".to_string(), vec![1]),
                ("DBMa".to_string(), vec![0]),
                ("LOCK".to_string(), vec![0, 0, 0, 0]),
            ]
        );
        assert_eq!(&*mode.borrow(), b"APP ");
    }

    #[test]
    fn leaves_a_controller_found_in_dbma_alone() {
        let (mode, log) = session(b"DBMa");
        assert!(log.borrow().is_empty());
        assert_eq!(&*mode.borrow(), b"DBMa");
    }

    /// A register-level transport (like raw i2c): CMD1 completes as soon as it is written,
    /// MODE follows DBMa only after `mode_lag` further reads, and every VDMs is answered.
    struct RegBus {
        cmd1: [u8; 4],
        mode: [u8; 4],
        pending_mode: Option<([u8; 4], usize)>,
        mode_lag: usize,
        rx_status: [u8; 5],
        data1: Vec<u8>,
        log: CommandLog,
    }

    impl RegBus {
        fn new(cmd1: &[u8; 4], mode: &[u8; 4], mode_lag: usize, log: CommandLog) -> Self {
            RegBus {
                cmd1: *cmd1,
                mode: *mode,
                pending_mode: None,
                mode_lag,
                rx_status: [0; 5],
                data1: Vec::new(),
                log,
            }
        }
    }

    impl BusDevice for RegBus {
        fn write_block(&mut self, reg: u8, data: &[u8]) -> Result<()> {
            match reg {
                TPS_REG_DATA1 => self.data1 = data.to_vec(),
                TPS_REG_CMD1 => {
                    let data1 = std::mem::take(&mut self.data1);
                    if data == b"DBMa" {
                        let mode = if data1[0] != 0 { *b"DBMa" } else { *b"APP " };
                        self.pending_mode = Some((mode, self.mode_lag));
                    }
                    if data == b"VDMs" {
                        self.rx_status[0] = self.rx_status[0].wrapping_add(1);
                        let header = u32::from_le_bytes(data1[1..5].try_into().unwrap()) | 0x40;
                        self.rx_status[1..5].copy_from_slice(&header.to_le_bytes());
                    }
                    self.log
                        .borrow_mut()
                        .push((String::from_utf8_lossy(data).into_owned(), data1));
                    self.cmd1 = [0; 4];
                }
                _ => return Err(Error::InvalidArgument),
            }
            Ok(())
        }

        fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()> {
            buf.fill(0);
            match reg {
                TPS_REG_CMD1 => buf.copy_from_slice(&self.cmd1),
                TPS_REG_MODE => {
                    if let Some((mode, left)) = self.pending_mode {
                        if left == 0 {
                            self.mode = mode;
                            self.pending_mode = None;
                        } else {
                            self.pending_mode = Some((mode, left - 1));
                        }
                    }
                    buf.copy_from_slice(&self.mode);
                }
                TPS_REG_VDM_RX_STATUS => buf[..5].copy_from_slice(&self.rx_status),
                _ => {}
            }
            Ok(())
        }
    }

    fn commands(log: &CommandLog) -> Vec<String> {
        log.borrow().iter().map(|(cmd, _)| cmd.clone()).collect()
    }

    #[test]
    fn a_stale_invalid_command_in_cmd1_is_idle() {
        let log = CommandLog::default();
        let bus = RegBus::new(b"!CMD", b"APP ", 0, log.clone());
        let device = Device::new(Box::new(bus), "J700".to_string()).unwrap();
        drop(device);
        assert_eq!(commands(&log), ["LOCK", "DBMa", "DBMa", "LOCK"]);
    }

    #[test]
    fn waits_for_mode_to_follow_dbma() {
        let log = CommandLog::default();
        let bus = RegBus::new(&[0; 4], b"APP ", 3, log.clone());
        let mut device = Device::new(Box::new(bus), "J700".to_string()).unwrap();
        device.serial().unwrap();
        drop(device);
        assert_eq!(
            commands(&log),
            ["LOCK", "DBMa", "VDMs", "DVEn", "DBMa", "LOCK"]
        );
    }

    #[test]
    fn mode_that_never_switches_is_an_error() {
        let log = CommandLog::default();
        let bus = RegBus::new(&[0; 4], b"APP ", usize::MAX, log.clone());
        assert!(matches!(
            Device::new(Box::new(bus), "J700".to_string()),
            Err(Error::TypecController)
        ));
    }

    /// A command-level transport whose first VDMs misses its budget.
    struct SlowVdmBus {
        inner: RegBus,
        vdms_timeouts: usize,
    }

    impl BusDevice for SlowVdmBus {
        fn write_block(&mut self, reg: u8, data: &[u8]) -> Result<()> {
            self.inner.write_block(reg, data)
        }

        fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()> {
            self.inner.read_block(reg, buf)
        }

        fn exec_cmd(&mut self, cmd: &[u8; 4], data: &[u8]) -> Option<Result<()>> {
            if cmd == b"VDMs" && self.vdms_timeouts > 0 {
                self.vdms_timeouts -= 1;
                self.inner
                    .log
                    .borrow_mut()
                    .push(("VDMs-timeout".into(), data.to_vec()));
                return Some(Err(Error::ControllerTimeout));
            }
            let write = self
                .inner
                .write_block(TPS_REG_DATA1, data)
                .and_then(|_| self.inner.write_block(TPS_REG_CMD1, cmd));
            Some(write)
        }
    }

    #[test]
    fn a_vdms_that_misses_its_budget_is_sent_again() {
        let log = CommandLog::default();
        let bus = SlowVdmBus {
            inner: RegBus::new(&[0; 4], b"DBMa", 0, log.clone()),
            vdms_timeouts: 2,
        };
        let mut device = Device::new(Box::new(bus), "J700".to_string()).unwrap();
        device.debugusb().unwrap();
        assert_eq!(commands(&log), ["VDMs-timeout", "VDMs-timeout", "VDMs"]);
    }
}
