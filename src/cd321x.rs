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

const RECONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_WAIT: Duration = Duration::from_millis(100);
const RECONNECT_WAIT: Duration = Duration::from_secs(1);
const CMD_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

const TPS_REG_MODE: u8 = 0x03;
const TPS_REG_CMD1: u8 = 0x08;
const TPS_REG_DATA1: u8 = 0x09;
const TPS_REG_POWER_STATUS: u8 = 0x3f;
const TPS_REG_VDM_RX_STATUS: u8 = 0x4d;
// 1 RX status/sequence byte + up to 7 4-byte VDOs (USB PD max VDM payload).
const TPS_REG_VDM_RX_STATUS_LEN: usize = 1 + 7 * 4;

const VDM_REPLY_TIMEOUT: Duration = Duration::from_secs(2);

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
        self.exec_cmd_with_timing(cmd_tag, in_data, Duration::from_secs(1), Duration::ZERO)
    }

    fn exec_cmd_with_timing(
        &mut self,
        cmd_tag: &[u8; 4],
        in_data: &[u8],
        cmd_timeout: Duration,
        res_delay: Duration,
    ) -> Result<()> {
        // A transport that executes commands itself owns CMD1 and its timeouts.
        if let Some(result) = self.bus_dev.exec_cmd(cmd_tag, in_data) {
            result?;
            thread::sleep(res_delay);
            return Ok(());
        }

        // First: wait for CMD1 to become idle.  During a reconnect the PD
        // controller can still be finishing its own VDM traffic; treating
        // that normal transient as a hard error loses the early boot window.
        let idle_start = Instant::now();
        loop {
            let mut status_buf = [0u8; 4];
            self.bus_dev.read_block(TPS_REG_CMD1, &mut status_buf)?;
            let val = u32::from_le_bytes(status_buf);
            if val == 0 {
                break;
            }
            if is_invalid_cmd(val) {
                info!("Invalid Command");
                return Err(Error::TypecController);
            }
            if idle_start.elapsed() > CMD_IDLE_TIMEOUT {
                info!("Busy Check Timed Out with VAL = {val:#x}");
                return Err(Error::ControllerTimeout);
            }
            thread::sleep(Duration::from_millis(10));
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
        thread::sleep(res_delay);
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
        let expected = if debug { TpsMode::Dbma } else { TpsMode::App };
        if self.get_mode()? != expected {
            return Err(Error::TypecController);
        }
        Ok(())
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
        let mut reply = [0u8; TPS_REG_VDM_RX_STATUS_LEN];
        self.bus_dev.read_block(TPS_REG_VDM_RX_STATUS, &mut reply)?;
        let rx_status = reply[0];
        let data = [
            vec![((sop as u8) << 4) | vdos.len() as u8],
            vdos.iter().flat_map(|val| val.to_le_bytes()).collect(),
        ]
        .concat();
        self.exec_cmd_with_timing(b"VDMs", &data, Duration::from_millis(200), Duration::ZERO)?;

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
            thread::sleep(Duration::from_millis(10));
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
        thread::sleep(RECONNECT_WAIT);
        let reconnect_start = Instant::now();
        loop {
            if self.check_connected().unwrap_or(false) {
                break;
            }
            thread::sleep(POLL_WAIT);
            if reconnect_start.elapsed() > RECONNECT_TIMEOUT {
                error!("Target did not reconnect after reboot");
                return Err(Error::ReconnectTimeout);
            }
        }
        thread::sleep(RECONNECT_WAIT);
        info!("Target reconnected");
        Ok(())
    }

    pub(crate) fn serial(&mut self) -> Result<()> {
        let vdos: [u32; 2] = [0x5ac8012, 0x1840306];
        info!("Putting target into serial mode...");
        let start = Instant::now();
        loop {
            match self.vdms(VdmSopType::SopStar, &vdos) {
                Ok(()) => break,
                Err(Error::ControllerTimeout) if start.elapsed() < RECONNECT_TIMEOUT => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err),
            }
        }
        info!("Putting local end into serial mode... ");
        if self.get_mode()? != TpsMode::Dbma {
            return Err(Error::TypecController);
        }
        self.dven(&vdos[1..2])
    }

    pub(crate) fn debugusb(&mut self) -> Result<()> {
        let vdos: [u32; 2] = [0x5ac8012, 0x1824606];
        info!("Putting target into DebugUSB mode...");
        let start = Instant::now();
        loop {
            match self.vdms(VdmSopType::SopStar, &vdos) {
                Ok(()) => return Ok(()),
                Err(Error::ControllerTimeout) if start.elapsed() < RECONNECT_TIMEOUT => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err),
            }
        }
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

    enum Reply {
        Ack,
        WrongHeader,
        StaleAck,
    }

    /// A command-level transport (like the kernel tipd interface) that records commands
    /// and models MODE for LOCK/DBMa.
    struct MockBus {
        mode: Mode,
        log: CommandLog,
        reply: Reply,
        vdm_rx: Rc<RefCell<[u8; TPS_REG_VDM_RX_STATUS_LEN]>>,
        vdm_rx_reads: Rc<RefCell<Vec<usize>>>,
    }

    impl BusDevice for MockBus {
        fn write_block(&mut self, _reg: u8, _data: &[u8]) -> Result<()> {
            Err(Error::FeatureMissing)
        }

        fn read_block(&mut self, reg: u8, buf: &mut [u8]) -> Result<()> {
            buf.fill(0);
            if reg == TPS_REG_MODE {
                buf.copy_from_slice(&*self.mode.borrow());
            } else if reg == TPS_REG_VDM_RX_STATUS {
                self.vdm_rx_reads.borrow_mut().push(buf.len());
                if buf.len() > TPS_REG_VDM_RX_STATUS_LEN {
                    return Err(Error::Io(std::io::Error::from_raw_os_error(5)));
                }
                buf.copy_from_slice(&self.vdm_rx.borrow()[..buf.len()]);
            }
            Ok(())
        }

        fn exec_cmd(&mut self, cmd: &[u8; 4], data: &[u8]) -> Option<Result<()>> {
            if cmd == b"DBMa" {
                *self.mode.borrow_mut() = if data[0] != 0 { *b"DBMa" } else { *b"APP " };
            } else if cmd == b"VDMs" && data.len() >= 5 {
                let mut rx = self.vdm_rx.borrow_mut();
                let request = u32::from_le_bytes(data[1..5].try_into().unwrap());
                let ack = request | 0x40;
                match self.reply {
                    Reply::Ack => {
                        rx[0] = rx[0].wrapping_add(1);
                        rx[1..5].copy_from_slice(&ack.to_le_bytes());
                    }
                    Reply::WrongHeader => {
                        rx[0] = rx[0].wrapping_add(1);
                        rx[1..5].copy_from_slice(&(ack ^ 1).to_le_bytes());
                    }
                    Reply::StaleAck => {}
                }
            }
            self.log
                .borrow_mut()
                .push((String::from_utf8_lossy(cmd).into_owned(), data.to_vec()));
            Some(Ok(()))
        }
    }

    fn mock_bus(mode: &[u8; 4]) -> (MockBus, Mode, CommandLog, Rc<RefCell<Vec<usize>>>) {
        let mode = Rc::new(RefCell::new(*mode));
        let log = Rc::new(RefCell::new(Vec::new()));
        let vdm_rx = Rc::new(RefCell::new([0u8; TPS_REG_VDM_RX_STATUS_LEN]));
        let vdm_rx_reads = Rc::new(RefCell::new(Vec::new()));
        let bus = MockBus {
            mode: mode.clone(),
            log: log.clone(),
            reply: Reply::Ack,
            vdm_rx,
            vdm_rx_reads: vdm_rx_reads.clone(),
        };
        (bus, mode, log, vdm_rx_reads)
    }

    fn session(mode: &[u8; 4]) -> (Mode, CommandLog) {
        let (bus, mode, log, _) = mock_bus(mode);
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

    #[test]
    fn vdms_reads_29_byte_rx_status_register() {
        let (bus, _, _, vdm_rx_reads) = mock_bus(b"APP ");
        let mut device = Device::new(Box::new(bus), "J713".to_string()).unwrap();
        device.reboot().unwrap();
        assert_eq!(*vdm_rx_reads.borrow(), vec![29, 29]);
    }

    #[test]
    fn vdms_accepts_ack_after_rx_sequence_wraps() {
        let (bus, _, _, reads) = mock_bus(b"DBMa");
        bus.vdm_rx.borrow_mut()[0] = u8::MAX;
        let mut device = Device::new(Box::new(bus), "J713".to_string()).unwrap();

        device.vdms(VdmSopType::Sop, &[0x1234_8001]).unwrap();
        assert_eq!(*reads.borrow(), vec![29, 29]);
    }

    #[test]
    fn vdms_rejects_a_fresh_reply_for_a_different_command() {
        let (mut bus, _, _, _) = mock_bus(b"DBMa");
        bus.reply = Reply::WrongHeader;
        let mut device = Device::new(Box::new(bus), "J713".to_string()).unwrap();

        assert!(matches!(
            device.vdms(VdmSopType::Sop, &[0x1234_8001]),
            Err(Error::VdmRejected)
        ));
    }

    #[test]
    fn vdms_times_out_with_an_ack_from_the_previous_exchange() {
        let (mut bus, _, _, _) = mock_bus(b"DBMa");
        bus.reply = Reply::StaleAck;
        bus.vdm_rx.borrow_mut()[1..5].copy_from_slice(&0x1234_8041u32.to_le_bytes());
        let mut device = Device::new(Box::new(bus), "J713".to_string()).unwrap();

        assert!(matches!(
            device.vdms(VdmSopType::Sop, &[0x1234_8001]),
            Err(Error::VdmReplyTimeout)
        ));
    }
}
