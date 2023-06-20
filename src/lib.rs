//! A library for running a USB/IP server

use log::*;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use rusb::*;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io::{ErrorKind, Result};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio::sync::{Barrier, watch};

pub mod cdc;
mod consts;
mod device;
mod endpoint;
pub mod hid;
mod host;
mod interface;
mod setup;
mod util;
pub use consts::*;
pub use device::*;
pub use endpoint::*;
pub use host::*;
pub use interface::*;
pub use setup::*;
pub use util::*;

/// Main struct of a USB/IP server
pub struct UsbIpServer {
    devices: Vec<UsbDevice>,
    control_barrier: Barrier,
    control_channel: (watch::Sender<bool>, watch::Receiver<bool>),
}

impl Default for UsbIpServer {
    fn default() -> Self {
        Self {
            devices: vec![],
            control_barrier: Barrier::new(1),
            control_channel: watch::channel(false),
        }
    }
}

impl UsbIpServer {
    /// Create a [`UsbIpServer`] with simulated devices
    pub fn new_simulated(devices: Vec<UsbDevice>) -> Self {
        Self {
            devices,
            ..Default::default()
        }
    }

    fn with_devices(device_list: Vec<Device<GlobalContext>>) -> Vec<UsbDevice> {
        let mut devices = vec![];

        for dev in device_list {
            let open_device = match dev.open() {
                Ok(dev) => dev,
                Err(err) => {
                    println!("Impossible to share {:?}: {}", dev, err);
                    continue;
                }
            };
            let handle = Arc::new(Mutex::new(open_device));
            let desc = dev.device_descriptor().unwrap();
            let cfg = dev.active_config_descriptor().unwrap();
            let mut interfaces = vec![];
            handle
                .lock()
                .unwrap()
                .set_auto_detach_kernel_driver(true)
                .ok();
            for intf in cfg.interfaces() {
                // ignore alternate settings
                let intf_desc = intf.descriptors().next().unwrap();
                handle
                    .lock()
                    .unwrap()
                    .set_auto_detach_kernel_driver(true)
                    .ok();
                let mut endpoints = vec![];

                for ep_desc in intf_desc.endpoint_descriptors() {
                    endpoints.push(UsbEndpoint {
                        address: ep_desc.address(),
                        attributes: ep_desc.transfer_type() as u8,
                        max_packet_size: ep_desc.max_packet_size(),
                        interval: ep_desc.interval(),
                    });
                }

                let handler = Arc::new(Mutex::new(Box::new(UsbHostInterfaceHandler::new(
                    handle.clone(),
                ))
                    as Box<dyn UsbInterfaceHandler + Send>));
                interfaces.push(UsbInterface {
                    interface_class: intf_desc.class_code(),
                    interface_subclass: intf_desc.sub_class_code(),
                    interface_protocol: intf_desc.protocol_code(),
                    endpoints,
                    string_interface: intf_desc.description_string_index().unwrap_or(0),
                    class_specific_descriptor: Vec::from(intf_desc.extra()),
                    handler,
                });
            }
            let mut device = UsbDevice {
                path: format!(
                    "/sys/bus/{}/{}/{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_id: format!(
                    "{}-{}-{}",
                    dev.bus_number(),
                    dev.address(),
                    dev.port_number()
                ),
                bus_num: dev.bus_number() as u32,
                dev_num: dev.port_number() as u32,
                speed: dev.speed() as u32,
                vendor_id: desc.vendor_id(),
                product_id: desc.product_id(),
                device_class: desc.class_code(),
                device_subclass: desc.sub_class_code(),
                device_protocol: desc.protocol_code(),
                device_bcd: desc.device_version().into(),
                configuration_value: cfg.number(),
                num_configurations: desc.num_configurations(),
                ep0_in: UsbEndpoint {
                    address: 0x80,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                ep0_out: UsbEndpoint {
                    address: 0x00,
                    attributes: EndpointAttributes::Control as u8,
                    max_packet_size: desc.max_packet_size() as u16,
                    interval: 0,
                },
                interfaces,
                device_handler: Some(Arc::new(Mutex::new(Box::new(UsbHostDeviceHandler::new(
                    handle.clone(),
                ))))),
                usb_version: desc.usb_version().into(),
                ..UsbDevice::default()
            };

            // set strings
            if let Some(index) = desc.manufacturer_string_index() {
                device.string_manufacturer = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            if let Some(index) = desc.product_string_index() {
                device.string_product = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            if let Some(index) = desc.serial_number_string_index() {
                device.string_serial = device.new_string(
                    &handle
                        .lock()
                        .unwrap()
                        .read_string_descriptor_ascii(index)
                        .unwrap(),
                )
            }
            devices.push(device);
        }
        devices
    }

    /// Create a [`UsbIpServer`] exposing devices in the host, and redirect all USB transfers to them using libusb
    pub fn new_from_host() -> Self {
        match rusb::devices() {
            Ok(list) => {
                let mut devs = vec![];
                for d in list.iter() {
                    devs.push(d)
                }
                let device_count = devs.len();
                Self {
                    devices: Self::with_devices(devs),
                    control_barrier: Barrier::new(device_count + 1),
                    ..Default::default()
                }
            }
            Err(_) => Default::default(),
        }
    }

    pub fn new_from_host_with_filter<F>(filter: F) -> Self
    where
        F: FnMut(&Device<GlobalContext>) -> bool,
    {
        match rusb::devices() {
            Ok(list) => {
                let mut devs = vec![];
                for d in list.iter().filter(filter) {
                    devs.push(d)
                }
                let device_count = devs.len();
                Self {
                    devices: Self::with_devices(devs),
                    control_barrier: Barrier::new(device_count + 1),
                    ..Default::default()
                }
            }
            Err(_) => Default::default(),
        }
    }

    async fn pause_sockets(self: &mut Self) {
        self.control_channel.0.send(true).unwrap();
        self.control_barrier.wait().await;
    }

    async fn resume_sockets(self: &mut Self) {
        self.control_channel.0.send(false).unwrap();
    }

    /// Add a [`UsbDevice`] to the server.
    /// This method will temporarily block all socket communication.
    pub async fn add_device(self: &mut Self, device: &UsbDevice) {
        self.pause_sockets().await;
        self.devices.push(device.clone());
        self.control_barrier = Barrier::new(self.devices.len() + 1);
        self.resume_sockets().await;
    }

    /// Remove a [`UsbDevice`] from the server.
    /// This method will temporarily block all socket communication.
    pub async fn remove_device(self: &mut Self, device: &UsbDevice) {
        self.pause_sockets().await;
        self.devices.retain(|d| d.bus_id != device.bus_id);
        self.control_barrier = Barrier::new(self.devices.len() + 1);
        self.resume_sockets().await;
    }

    /// Start a loop that will handle communication for a single socket.
    /// 
    /// Returns `Ok(())` if the socket was closed by the remote, or an error otherwise.
    /// 
    /// This method will be blocked whenever [`UsbIpServer::add_device`] or [`UsbIpServer::remove_device`] is called.
    /// 
    /// See [`server`] for example usage.
    pub async fn handler<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        self: &Self,
        mut socket: &mut S,
    ) -> Result<()> {
        let mut current_import_device = None;
        loop {
            let should_stop_for_control = self.control_channel.1.borrow().clone();
            if should_stop_for_control {
                self.control_barrier.wait().await;
                self.control_channel.1.clone().wait_for(|&b| !b).await.unwrap();
            }

            let mut command = [0u8; 4];
            if let Err(err) = socket.read_exact(&mut command).await {
                if err.kind() == ErrorKind::UnexpectedEof {
                    info!("Remote closed the connection");
                    return Ok(());
                } else {
                    return Err(err);
                }
            }
            match command {
                [0x01, 0x11, 0x80, 0x05] => {
                    trace!("Got OP_REQ_DEVLIST");
                    let _status = socket.read_u32().await?;

                    // OP_REP_DEVLIST
                    socket.write_u32(0x01110005).await?;
                    socket.write_u32(0).await?;
                    socket.write_u32(self.devices.len() as u32).await?;
                    for dev in &self.devices {
                        dev.write_dev_with_interfaces(&mut socket).await?;
                    }
                    trace!("Sent OP_REP_DEVLIST");
                }
                [0x01, 0x11, 0x80, 0x03] => {
                    trace!("Got OP_REQ_IMPORT");
                    let _status = socket.read_u32().await?;
                    let mut bus_id = [0u8; 32];
                    socket.read_exact(&mut bus_id).await?;
                    current_import_device = None;
                    for device in &self.devices {
                        let mut expected = device.bus_id.as_bytes().to_vec();
                        expected.resize(32, 0);
                        if expected == bus_id {
                            current_import_device = Some(device);
                            info!("Found device {:?}", device.path);
                            break;
                        }
                    }

                    // OP_REP_IMPORT
                    trace!("Sent OP_REP_IMPORT");
                    socket.write_u32(0x01110003).await?;
                    if let Some(dev) = current_import_device {
                        socket.write_u32(0).await?;
                        dev.write_dev(&mut socket).await?;
                    } else {
                        socket.write_u32(1).await?;
                    }
                }
                [0x00, 0x00, 0x00, 0x01] => {
                    trace!("Got USBIP_CMD_SUBMIT");
                    let seq_num = socket.read_u32().await?;
                    let _dev_id = socket.read_u32().await?;
                    let direction = socket.read_u32().await?;
                    let ep = socket.read_u32().await?;
                    let _transfer_flags = socket.read_u32().await?;
                    let transfer_buffer_length = socket.read_u32().await?;
                    let _start_frame = socket.read_u32().await?;
                    let _number_of_packets = socket.read_u32().await?;
                    let _interval = socket.read_u32().await?;
                    let mut setup = [0u8; 8];
                    socket.read_exact(&mut setup).await?;
                    let device = current_import_device.unwrap();

                    let out = direction == 0;
                    let real_ep = if out { ep } else { ep | 0x80 };
                    // read request data from socket for OUT
                    let out_data = if out {
                        let mut data = vec![0u8; transfer_buffer_length as usize];
                        socket.read_exact(&mut data).await?;
                        data
                    } else {
                        vec![]
                    };

                    let (usb_ep, intf) = device.find_ep(real_ep as u8).unwrap();
                    trace!("->Endpoint {:02x?}", usb_ep);
                    trace!("->Setup {:02x?}", setup);
                    trace!("->Request {:02x?}", out_data);
                    let resp = device
                        .handle_urb(usb_ep, intf, SetupPacket::parse(&setup), &out_data)
                        .await?;

                    if out {
                        trace!("<-Resp {:02x?}", resp);
                    } else {
                        trace!("<-Wrote {}", out_data.len());
                    }

                    // USBIP_RET_SUBMIT
                    // command
                    socket.write_u32(0x3).await?;
                    socket.write_u32(seq_num).await?;
                    socket.write_u32(0).await?;
                    socket.write_u32(0).await?;
                    socket.write_u32(0).await?;
                    // status
                    socket.write_u32(0).await?;

                    let actual_length = if out {
                        // In the out endpoint case, the actual_length field should be
                        // same as the data length received in the original URB transaction.
                        // No data bytes are sent
                        transfer_buffer_length as u32
                    } else {
                        resp.len() as u32
                    };
                    // actual_length
                    socket.write_u32(actual_length).await?;

                    // start frame
                    socket.write_u32(0).await?;
                    // number of packets
                    socket.write_u32(0).await?;
                    // error count
                    socket.write_u32(0).await?;
                    // padding
                    let padding = [0u8; 8];
                    socket.write_all(&padding).await?;
                    // data
                    if !out {
                        socket.write_all(&resp).await?;
                    }
                }
                [0x00, 0x00, 0x00, 0x02] => {
                    trace!("Got USBIP_CMD_UNLINK");
                    let seq_num = socket.read_u32().await?;
                    let _dev_id = socket.read_u32().await?;
                    let _direction = socket.read_u32().await?;
                    let _ep = socket.read_u32().await?;
                    let _seq_num_submit = socket.read_u32().await?;
                    // 24 bytes of struct padding
                    let mut padding = [0u8; 6 * 4];
                    socket.read_exact(&mut padding).await?;

                    // USBIP_RET_UNLINK
                    // command
                    socket.write_u32(0x4).await?;
                    socket.write_u32(seq_num).await?;
                    socket.write_u32(0).await?;
                    socket.write_u32(0).await?;
                    socket.write_u32(0).await?;
                    // status
                    socket.write_u32(0).await?;
                    socket.write_all(&mut padding).await?;
                }
                _ => warn!("Got unknown command {:?}", command),
            }
        }
    }
}

/// Spawn a USB/IP server at `addr` using [TcpListener]
pub async fn server(addr: SocketAddr, server: UsbIpServer) {
    let listener = TcpListener::bind(addr).await.expect("bind to addr");

    let server = async move {
        let usbip_server = Arc::new(server);
        loop {
            match listener.accept().await {
                Ok((mut socket, _addr)) => {
                    info!("Got connection from {:?}", socket.peer_addr());
                    let new_server = usbip_server.clone();
                    tokio::spawn(async move {
                        let res = new_server.handler(&mut socket).await;
                        info!("Handler ended with {:?}", res);
                    });
                }
                Err(err) => {
                    warn!("Got error {:?}", err);
                }
            }
        }
    };

    server.await
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::util::tests::*;

    #[tokio::test]
    async fn req_empty_devlist() {
        let server: UsbIpServer = Default::default();

        // OP_REQ_DEVLIST
        let mut mock_socket = MockSocket::new(vec![0x01, 0x11, 0x80, 0x05, 0x00, 0x00, 0x00, 0x00]);
        server.handler(&mut mock_socket).await.ok();
        // OP_REP_DEVLIST
        assert_eq!(
            mock_socket.output,
            [0x01, 0x11, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[tokio::test]
    async fn req_sample_devlist() {
        let intf_handler = Arc::new(Mutex::new(
            Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
        ));
        let server = UsbIpServer {
            devices: vec![UsbDevice::new(0).with_interface(
                ClassCode::CDC as u8,
                cdc::CDC_ACM_SUBCLASS,
                0x00,
                "Test CDC ACM",
                cdc::UsbCdcAcmHandler::endpoints(),
                intf_handler.clone(),
            )],
            ..Default::default()
        };

        // OP_REQ_DEVLIST
        let mut mock_socket = MockSocket::new(vec![0x01, 0x11, 0x80, 0x05, 0x00, 0x00, 0x00, 0x00]);
        server.handler(&mut mock_socket).await.ok();
        // OP_REP_DEVLIST
        // header: 0xC
        // device: 0x138
        // interface: 4 * 0x1
        assert_eq!(mock_socket.output.len(), 0xC + 0x138 + 4 * 0x1);
    }

    #[tokio::test]
    async fn req_import() {
        let intf_handler = Arc::new(Mutex::new(
            Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
        ));
        let server = UsbIpServer {
            devices: vec![UsbDevice::new(0).with_interface(
                ClassCode::CDC as u8,
                cdc::CDC_ACM_SUBCLASS,
                0x00,
                "Test CDC ACM",
                cdc::UsbCdcAcmHandler::endpoints(),
                intf_handler.clone(),
            )],
            ..Default::default()
        };

        // OP_REQ_IMPORT
        let mut req = vec![0x01, 0x11, 0x80, 0x03, 0x00, 0x00, 0x00, 0x00];
        let mut path = "0".as_bytes().to_vec();
        path.resize(32, 0);
        req.extend(path);
        let mut mock_socket = MockSocket::new(req);
        server.handler(&mut mock_socket).await.ok();
        // OP_REQ_IMPORT
        assert_eq!(mock_socket.output.len(), 0x140);
    }

    #[tokio::test]
    async fn req_import_get_device_desc() {
        let intf_handler = Arc::new(Mutex::new(
            Box::new(cdc::UsbCdcAcmHandler::new()) as Box<dyn UsbInterfaceHandler + Send>
        ));
        let server = UsbIpServer {
            devices: vec![UsbDevice::new(0).with_interface(
                ClassCode::CDC as u8,
                cdc::CDC_ACM_SUBCLASS,
                0x00,
                "Test CDC ACM",
                cdc::UsbCdcAcmHandler::endpoints(),
                intf_handler.clone(),
            )],
            ..Default::default()
        };

        // OP_REQ_IMPORT
        let mut req = vec![0x01, 0x11, 0x80, 0x03, 0x00, 0x00, 0x00, 0x00];
        let mut path = "0".as_bytes().to_vec();
        path.resize(32, 0);
        req.extend(path);
        // USBIP_CMD_SUBMIT
        req.extend(vec![
            0x00, 0x00, 0x00, 0x01, // command
            0x00, 0x00, 0x00, 0x01, // seq num
            0x00, 0x00, 0x00, 0x00, // dev id
            0x00, 0x00, 0x00, 0x01, // IN
            0x00, 0x00, 0x00, 0x00, // ep 0
            0x00, 0x00, 0x00, 0x00, // transfer flags
            0x00, 0x00, 0x00, 0x00, // transfer buffer length
            0x00, 0x00, 0x00, 0x00, // start frame
            0x00, 0x00, 0x00, 0x00, // number of packets
            0x00, 0x00, 0x00, 0x00, // interval
            0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x40, 0x00, // GetDescriptor to Device
        ]);
        let mut mock_socket = MockSocket::new(req);
        server.handler(&mut mock_socket).await.ok();
        // OP_REQ_IMPORT + USBIP_CMD_SUBMIT + Device Descriptor
        assert_eq!(mock_socket.output.len(), 0x140 + 0x30 + 0x12);
    }
}
