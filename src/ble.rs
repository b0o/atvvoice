use std::pin::Pin;

use anyhow::{Context, Result};
use bluer::{gatt::remote::Characteristic, Adapter, AdapterEvent, Address, Device, Uuid};
use futures::{Stream, StreamExt};

use crate::atvv::{BleDevice, BleFut, BleStream, DeviceConnectionEvent};

/// Wrap a `CharacteristicReader` (from `AcquireNotify`) into a `Stream<Item = Vec<u8>>`.
fn reader_to_stream(
    reader: bluer::gatt::CharacteristicReader,
) -> Pin<Box<dyn Stream<Item = Vec<u8>> + Send>> {
    Box::pin(futures::stream::unfold(reader, |reader| async move {
        match reader.recv().await {
            Ok(data) => Some((data, reader)),
            Err(_) => None, // FD closed (device disconnected)
        }
    }))
}

/// ATVV Service UUID: AB5E0001-5A21-4F05-BC7D-AF01F617B664
pub const ATVV_SERVICE: Uuid = Uuid::from_u128(0xab5e0001_5a21_4f05_bc7d_af01f617b664);

/// ATVV TX Characteristic (Host → Remote): AB5E0002
pub const ATVV_CHAR_TX: Uuid = Uuid::from_u128(0xab5e0002_5a21_4f05_bc7d_af01f617b664);

/// ATVV RX Characteristic (Remote → Host, audio): AB5E0003
pub const ATVV_CHAR_RX: Uuid = Uuid::from_u128(0xab5e0003_5a21_4f05_bc7d_af01f617b664);

/// ATVV CTL Characteristic (Remote → Host, control): AB5E0004
pub const ATVV_CHAR_CTL: Uuid = Uuid::from_u128(0xab5e0004_5a21_4f05_bc7d_af01f617b664);

/// Resolved ATVV characteristics for a connected device.
pub struct AtvvChars {
    pub tx: Characteristic,
    pub rx: Characteristic,
    pub ctl: Characteristic,
}

/// Real BLE device implementation wrapping bluer types.
/// Borrows the device and characteristics so main.rs retains ownership
/// for reconnect logic and shutdown MIC_CLOSE.
pub struct BluerDevice<'a> {
    pub device: &'a Device,
    pub chars: &'a AtvvChars,
}

impl BleDevice for BluerDevice<'_> {
    fn write_command(&self, data: &[u8]) -> BleFut<'_, ()> {
        let data = data.to_vec();
        Box::pin(async move {
            self.chars.tx.write(&data).await?;
            Ok(())
        })
    }

    fn ctl_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>> {
        Box::pin(async {
            let reader = self.chars.ctl.notify_io().await
                .context("Failed to acquire exclusive CTL notifications. \
                          Another ATVVoice instance may be connected to this device.")?;
            tracing::debug!("CTL AcquireNotify: exclusive access, MTU={}", reader.mtu());
            Ok(reader_to_stream(reader))
        })
    }

    fn rx_notifications(&self) -> BleFut<'_, BleStream<Vec<u8>>> {
        Box::pin(async {
            let reader = self.chars.rx.notify_io().await
                .context("Failed to acquire exclusive RX notifications. \
                          Another ATVVoice instance may be connected to this device.")?;
            tracing::debug!("RX AcquireNotify: exclusive access, MTU={}", reader.mtu());
            Ok(reader_to_stream(reader))
        })
    }

    fn connection_events(&self) -> BleFut<'_, BleStream<DeviceConnectionEvent>> {
        Box::pin(async {
            let stream = self.device.events().await?;
            let mapped = stream.filter_map(|event| async move {
                if let bluer::DeviceEvent::PropertyChanged(
                    bluer::DeviceProperty::Connected(false),
                ) = event
                {
                    Some(DeviceConnectionEvent::Disconnected)
                } else {
                    None
                }
            });
            Ok(
                Box::pin(mapped) as BleStream<DeviceConnectionEvent>,
            )
        })
    }
}

/// Find a bonded device that advertises the ATVV service.
/// If `filter_addr` is Some, only match that specific address.
/// Addresses in `exclude` are skipped (e.g. devices locked by another instance).
pub async fn find_atvv_device(
    adapter: &Adapter,
    filter_addr: Option<Address>,
    exclude: &[Address],
) -> Result<Device> {
    // First check already-known devices
    for addr in adapter.device_addresses().await? {
        if exclude.contains(&addr) {
            continue;
        }
        if let Some(filter) = filter_addr {
            if addr != filter {
                continue;
            }
        }
        let device = adapter.device(addr)?;
        if let Ok(Some(uuids)) = device.uuids().await {
            if uuids.contains(&ATVV_SERVICE) {
                tracing::info!(
                    "Found ATVV device: {} ({})",
                    device.name().await?.unwrap_or_default(),
                    addr
                );
                return Ok(device);
            }
        }
    }

    // Fall back to discovery stream
    tracing::info!("Scanning for ATVV devices...");
    let discover = adapter.discover_devices().await?;
    tokio::pin!(discover);
    while let Some(evt) = discover.next().await {
        if let AdapterEvent::DeviceAdded(addr) = evt {
            if exclude.contains(&addr) {
                continue;
            }
            if let Some(filter) = filter_addr {
                if addr != filter {
                    continue;
                }
            }
            let device = adapter.device(addr)?;
            if let Ok(Some(uuids)) = device.uuids().await {
                if uuids.contains(&ATVV_SERVICE) {
                    tracing::info!("Discovered ATVV device: {}", addr);
                    return Ok(device);
                }
            }
        }
    }

    anyhow::bail!("No ATVV device found")
}

/// Resolve the three ATVV GATT characteristics from a connected device.
pub async fn resolve_chars(device: &Device) -> Result<AtvvChars> {
    let mut tx = None;
    let mut rx = None;
    let mut ctl = None;

    for service in device.services().await? {
        if service.uuid().await? != ATVV_SERVICE {
            continue;
        }

        for char in service.characteristics().await? {
            match char.uuid().await? {
                uuid if uuid == ATVV_CHAR_TX => tx = Some(char),
                uuid if uuid == ATVV_CHAR_RX => rx = Some(char),
                uuid if uuid == ATVV_CHAR_CTL => ctl = Some(char),
                _ => {}
            }
        }
    }

    Ok(AtvvChars {
        tx: tx.context("ATVV TX characteristic not found")?,
        rx: rx.context("ATVV RX characteristic not found")?,
        ctl: ctl.context("ATVV CTL characteristic not found")?,
    })
}
