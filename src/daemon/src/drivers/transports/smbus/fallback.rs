//! Stub SMBus backend for platforms without an i2c-dev or PawnIO/NvAPI path.
//! Every operation reports "not supported"; enumeration yields no buses.

use super::*;

pub(super) struct SmBusInner;

const UNSUPPORTED: &str = "SMBus not supported on this platform";

impl SmBusSyncOps for SmBusInner {
    fn read_byte(&mut self, _addr: u8) -> Result<u8> {
        Err(anyhow!(UNSUPPORTED))
    }
    fn read_byte_data(&mut self, _addr: u8, _cmd: u8) -> Result<u8> {
        Err(anyhow!(UNSUPPORTED))
    }
    fn write_quick(&mut self, _addr: u8) -> Result<bool> {
        Ok(false)
    }
    fn write_byte_data(&mut self, _addr: u8, _cmd: u8, _val: u8) -> Result<()> {
        Err(anyhow!(UNSUPPORTED))
    }
    fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> Result<()> {
        Err(anyhow!(UNSUPPORTED))
    }
    fn write_block_data(&mut self, _addr: u8, _cmd: u8, _data: &[u8]) -> Result<()> {
        Err(anyhow!(UNSUPPORTED))
    }
}

pub fn enumerate_buses() -> Vec<BusInfo> {
    vec![]
}
pub fn enumerate_gpu_buses() -> Vec<BusInfo> {
    vec![]
}
pub fn open_device(_info: &BusInfo) -> Result<SmBusInner> {
    Err(anyhow!("SMBus not supported on this platform"))
}
