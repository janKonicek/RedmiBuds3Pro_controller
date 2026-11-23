// SPDX-License-Identifier: AGPL-3.0-or-later
//! Windows transport: WinRT Bluetooth RFCOMM to the earbuds' control service,
//! plus a few connection diagnostics.

use windows::Devices::Bluetooth::Rfcomm::{RfcommDeviceService, RfcommServiceId};
use windows::Devices::Bluetooth::{BluetoothCacheMode, BluetoothDevice, BluetoothError};
use windows::Devices::Enumeration::DeviceInformation;
use windows::Networking::Sockets::{SocketProtectionLevel, StreamSocket};
use windows::Storage::Streams::{DataReader, DataWriter, InputStreamOptions};
use windows::core::{GUID, HSTRING, Result};

/// A paired Bluetooth device as seen by Windows.
#[derive(Debug, Clone)]
pub struct PairedDevice {
    pub name: String,
    pub id: HSTRING,
}

/// Lists paired Bluetooth Classic devices.
pub fn paired_devices() -> Result<Vec<PairedDevice>> {
    let selector = BluetoothDevice::GetDeviceSelectorFromPairingState(true)?;
    let list = DeviceInformation::FindAllAsyncAqsFilter(&selector)?.get()?;
    let mut out = Vec::new();
    for di in list {
        out.push(PairedDevice { name: di.Name()?.to_string(), id: di.Id()? });
    }
    Ok(out)
}

pub fn find_device(name_part: &str) -> std::result::Result<(PairedDevice, BluetoothDevice), ConnectError> {
    let dev = paired_devices()?
        .into_iter()
        .find(|d| d.name.contains(name_part))
        .ok_or_else(|| ConnectError::NotPaired(name_part.to_owned()))?;
    let bt = BluetoothDevice::FromIdAsync(&dev.id)?.get()?;
    Ok((dev, bt))
}

/// One RFCOMM service from an uncached SDP query.
#[derive(Debug, Clone)]
pub struct ServiceInfo {
    pub uuid: GUID,
    /// RFCOMM server channel from the SDP protocol descriptor list, if parsed.
    pub channel: Option<u8>,
}

/// Extracts the RFCOMM channel from SDP attribute 0x0004 (ProtocolDescriptorList):
/// look for UUID16 0x0003 (RFCOMM, `19 00 03`) followed by a UINT8 (`08 nn`).
pub fn parse_rfcomm_channel(attr: &[u8]) -> Option<u8> {
    attr.windows(5).find(|w| w[..4] == [0x19, 0x00, 0x03, 0x08]).map(|w| w[4])
}

fn sdp_channel(svc: &RfcommDeviceService) -> Option<u8> {
    let attrs = svc.GetSdpRawAttributesWithCacheModeAsync(BluetoothCacheMode::Uncached).ok()?.get().ok()?;
    let buf = attrs.Lookup(0x0004).ok()?;
    let reader = DataReader::FromBuffer(&buf).ok()?;
    let mut bytes = vec![0u8; buf.Length().ok()? as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    parse_rfcomm_channel(&bytes)
}

/// Diagnostic: all RFCOMM services the device exposes (uncached SDP query).
pub fn list_rfcomm_services(name_part: &str) -> std::result::Result<Vec<ServiceInfo>, ConnectError> {
    let (_, bt) = find_device(name_part)?;
    let res = bt.GetRfcommServicesWithCacheModeAsync(BluetoothCacheMode::Uncached)?.get()?;
    let mut out = Vec::new();
    for s in res.Services()? {
        out.push(ServiceInfo { uuid: s.ServiceId()?.Uuid()?, channel: sdp_channel(&s) });
    }
    Ok(out)
}

/// Diagnostic: raw Winsock AF_BTH RFCOMM connect, either by service UUID
/// (`port == 0`) or to an explicit RFCOMM channel. Closes right away.
pub fn winsock_connect_test(name_part: &str, uuid: GUID, port: u32) -> std::result::Result<(), String> {
    use windows::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH};
    use windows::Win32::Networking::WinSock::*;
    let (_, bt) = find_device(name_part).map_err(|e| e.to_string())?;
    let addr = bt.BluetoothAddress().map_err(|e| e.message().to_string())?;
    unsafe {
        let mut wsa = WSADATA::default();
        let r = WSAStartup(0x0202, &mut wsa);
        if r != 0 {
            return Err(format!("WSAStartup {r}"));
        }
        let s = socket(AF_BTH as i32, SOCK_STREAM, BTHPROTO_RFCOMM as i32).map_err(|e| format!("socket(): {e}"))?;
        let sa = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: addr,
            serviceClassId: if port == 0 { uuid } else { GUID::zeroed() },
            port,
        };
        let r = connect(s, &sa as *const _ as *const SOCKADDR, std::mem::size_of::<SOCKADDR_BTH>() as i32);
        let res = if r == 0 { Ok(()) } else { Err(format!("WSA error {}", WSAGetLastError().0)) };
        closesocket(s);
        res
    }
}

#[derive(Debug)]
pub enum ConnectError {
    NotPaired(String),
    /// GetRfcommServices returned a Bluetooth error (e.g. device unreachable).
    ServiceLookup(BluetoothError),
    ServiceNotFound,
    Win(windows::core::Error),
}

impl From<windows::core::Error> for ConnectError {
    fn from(e: windows::core::Error) -> Self {
        Self::Win(e)
    }
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPaired(n) => write!(f, "no paired device whose name contains {n:?}"),
            Self::ServiceLookup(e) => write!(f, "RFCOMM service lookup failed: BluetoothError({})", e.0),
            Self::ServiceNotFound => write!(f, "device does not expose the requested RFCOMM service"),
            Self::Win(e) => write!(f, "WinRT error 0x{:08X}: {}", e.code().0 as u32, e.message()),
        }
    }
}

pub struct Connection {
    pub device_name: String,
    socket: StreamSocket,
    reader: DataReader,
    writer: DataWriter,
}

impl Connection {
    /// Connects to the control service of the first paired device whose name contains `name_part`.
    pub fn connect(name_part: &str) -> std::result::Result<Self, ConnectError> {
        Self::connect_uuid(name_part, GUID::from_u128(crate::SERVICE_UUID), false)
    }

    /// Connects to an arbitrary RFCOMM service UUID. `verbose` logs each failed attempt.
    pub fn connect_uuid(name_part: &str, uuid: GUID, verbose: bool) -> std::result::Result<Self, ConnectError> {
        let (dev, bt) = find_device(name_part)?;
        let sid = RfcommServiceId::FromUuid(uuid)?;
        let res = bt.GetRfcommServicesForIdWithCacheModeAsync(&sid, BluetoothCacheMode::Uncached)?.get()?;
        let err = res.Error()?;
        if err != BluetoothError::Success {
            return Err(ConnectError::ServiceLookup(err));
        }
        let services = res.Services()?;
        if services.Size()? == 0 {
            return Err(ConnectError::ServiceNotFound);
        }
        let svc: RfcommDeviceService = services.GetAt(0)?;

        let host = svc.ConnectionHostName()?;
        let name = svc.ConnectionServiceName()?;
        let mut last_err = None;
        let mut socket = None;
        for level in [
            SocketProtectionLevel::PlainSocket,
            SocketProtectionLevel::BluetoothEncryptionAllowNullAuthentication,
            SocketProtectionLevel::BluetoothEncryptionWithAuthentication,
        ] {
            let s = StreamSocket::new()?;
            match s.ConnectWithProtectionLevelAsync(&host, &name, level)?.get() {
                Ok(()) => {
                    socket = Some(s);
                    break;
                }
                Err(e) => {
                    if verbose {
                        eprintln!("  connect with {level:?} failed: 0x{:08X} {}", e.code().0 as u32, e.message());
                    }
                    last_err = Some(e);
                }
            }
        }
        let socket = match socket {
            Some(s) => s,
            None => return Err(ConnectError::Win(last_err.expect("at least one attempt"))),
        };

        let reader = DataReader::CreateDataReader(&socket.InputStream()?)?;
        reader.SetInputStreamOptions(InputStreamOptions::Partial)?;
        let writer = DataWriter::CreateDataWriter(&socket.OutputStream()?)?;
        Ok(Self { device_name: dev.name, socket, reader, writer })
    }

    pub fn write(&self, data: &[u8]) -> Result<()> {
        self.writer.WriteBytes(data)?;
        self.writer.StoreAsync()?.get()?;
        Ok(())
    }

    /// Blocks until some bytes arrive (or the socket is closed). Returns 0 on EOF.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let n = self.reader.LoadAsync(buf.len() as u32)?.get()? as usize;
        if n > 0 {
            self.reader.ReadBytes(&mut buf[..n])?;
        }
        Ok(n)
    }

    /// A handle that can close the socket from another thread (unblocks `read`).
    pub fn closer(&self) -> Closer {
        Closer(self.socket.clone())
    }
}

#[derive(Clone)]
pub struct Closer(StreamSocket);

impl Closer {
    pub fn close(&self) {
        let _ = self.0.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfcomm_channel_from_protocol_descriptor_list() {
        // DES { DES { UUID16 L2CAP }, DES { UUID16 RFCOMM, UINT8 5 } }
        let attr = [0x35, 0x0C, 0x35, 0x03, 0x19, 0x01, 0x00, 0x35, 0x05, 0x19, 0x00, 0x03, 0x08, 0x05];
        assert_eq!(parse_rfcomm_channel(&attr), Some(5));
        assert_eq!(parse_rfcomm_channel(&[0x35, 0x03, 0x19, 0x01, 0x00]), None);
    }
}
