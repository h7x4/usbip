//! Implement CDC(Communications) device
use super::*;

/// A handler of a CDC ACM(Abstract Control Model)
#[derive(Clone)]
pub struct UsbCdcAcmHandler {
    pub tx_buffer: Vec<u8>,
}

// https://www.usb.org/sites/default/files/usbmassbulk_10.pdf

/// Sub class code for CDC ACM
pub const CDC_ACM_SUBCLASS: u8 = 0x02;