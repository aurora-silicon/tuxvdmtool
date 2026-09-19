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
}

impl Device {
    pub(crate) fn new_for_reset(bus_dev: Box<dyn BusDevice>, code: String) -> Self {
        Self {
            bus_dev,
            key: code.into_bytes().into_iter().rev().collect::<Vec<u8>>(),
            skip_shutdown: true,
        }
    }

    pub(crate) fn new(bus_dev: Box<dyn BusDevice>, code: String) -> Result<Self> {
        let mut device = Self {
            bus_dev,
            key: code.into_bytes().into_iter().rev().collect::<Vec<u8>>(),
            skip_shutdown: false,
        };
        match device.get_mode()? {
            TpsMode::App => {
                device.lock(device.key.clone().as_slice())?;
                device.dbma(true)?;
            }
            TpsMode::Dbma => {
                info!("Controller already in DBMa mode; reusing active session");
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
        if self.get_mode()? != TpsMode::Dbma {
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
        let mut reply = [0u8; 64];
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
        if self.skip_shutdown {
            return;
        }
        let lock: [u8; 4] = [0, 0, 0, 0];
        let _ = self.dbma(false);
        let _ = self.lock(&lock);
    }
}
