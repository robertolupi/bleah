use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::Duration;

use btleplug::api::{AddressType, Central as _, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub rssi: Option<i16>,
    pub connected: bool,
    pub tx_power_level: Option<i16>,
    pub address_type: Option<AddressType>,
    pub manufacturer_data: BTreeMap<u16, Vec<u8>>,
    pub service_data: BTreeMap<String, Vec<u8>>,
    pub services: Vec<String>,
}

#[derive(Debug)]
pub enum ScanMessage {
    Devices(Vec<DeviceInfo>),
    Status(String),
}

pub struct DetailItem {
    pub label: String,
    pub value: String,
}

pub trait PeripheralDecoder: Send + Sync {
    fn summary(&self, device: &DeviceInfo) -> Option<String>;
    fn details(&self, device: &DeviceInfo) -> Vec<DetailItem>;
}

pub fn default_decoders() -> Vec<Box<dyn PeripheralDecoder>> {
    vec![Box::new(RuuviDecoder)]
}

pub fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|byte| format!("{:02x}", byte))
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn scan_loop(tx: mpsc::Sender<ScanMessage>, mut shutdown: watch::Receiver<bool>) {
    let manager = match Manager::new().await {
        Ok(manager) => manager,
        Err(err) => {
            let _ = tx.send(ScanMessage::Status(format!("BLE manager error: {err}")));
            return;
        }
    };

    let adapters = match manager.adapters().await {
        Ok(adapters) => adapters,
        Err(err) => {
            let _ = tx.send(ScanMessage::Status(format!("Adapter discovery error: {err}")));
            return;
        }
    };

    let Some(adapter) = adapters.into_iter().next() else {
        let _ = tx.send(ScanMessage::Status("No BLE adapters found".to_string()));
        return;
    };

    if let Err(err) = adapter.start_scan(ScanFilter::default()).await {
        let _ = tx.send(ScanMessage::Status(format!("Scan failed: {err}")));
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_secs(2));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
                }
            }
            _ = interval.tick() => {
                let peripherals = match adapter.peripherals().await {
                    Ok(peripherals) => peripherals,
                    Err(err) => {
                        let _ = tx.send(ScanMessage::Status(format!("Scan error: {err}")));
                        continue;
                    }
                };

                let mut devices = Vec::new();
                for peripheral in peripherals {
                    let id = peripheral.id().to_string();
                    let props = peripheral.properties().await.ok().flatten();
                    let name = props
                        .as_ref()
                        .and_then(|props| props.local_name.clone())
                        .unwrap_or_else(|| "Unknown".to_string());
                    let rssi = props.as_ref().and_then(|props| props.rssi);
                    let connected = peripheral.is_connected().await.unwrap_or(false);
                    let tx_power_level = props.as_ref().and_then(|props| props.tx_power_level);
                    let address_type = props.as_ref().and_then(|props| props.address_type);
                    let manufacturer_data = props
                        .as_ref()
                        .map(|props| {
                            props
                                .manufacturer_data
                                .iter()
                                .map(|(key, value)| (*key, value.clone()))
                                .collect::<BTreeMap<_, _>>()
                        })
                        .unwrap_or_default();
                    let service_data = props
                        .as_ref()
                        .map(|props| {
                            props
                                .service_data
                                .iter()
                                .map(|(key, value)| (key.to_string(), value.clone()))
                                .collect::<BTreeMap<_, _>>()
                        })
                        .unwrap_or_default();
                    let services = props
                        .as_ref()
                        .map(|props| props.services.iter().map(|uuid| uuid.to_string()).collect())
                        .unwrap_or_default();

                    devices.push(DeviceInfo {
                        id,
                        name,
                        rssi,
                        connected,
                        tx_power_level,
                        address_type,
                        manufacturer_data,
                        service_data,
                        services,
                    });
                }

                let _ = tx.send(ScanMessage::Devices(devices));
            }
        }
    }
}

struct RuuviDecoder;

impl RuuviDecoder {
    fn decode_format5(data: &[u8]) -> Option<(Option<f32>, Option<f32>)> {
        if data.len() < 5 || data[0] != 0x05 {
            return None;
        }

        let temp_raw = i16::from_be_bytes([data[1], data[2]]);
        let humidity_raw = u16::from_be_bytes([data[3], data[4]]);
        let temp = if temp_raw == i16::MIN {
            None
        } else {
            Some(f32::from(temp_raw) * 0.005)
        };
        let humidity = if humidity_raw == u16::MAX {
            None
        } else {
            Some(f32::from(humidity_raw) * 0.0025)
        };
        Some((temp, humidity))
    }
}

impl PeripheralDecoder for RuuviDecoder {
    fn summary(&self, device: &DeviceInfo) -> Option<String> {
        let data = device.manufacturer_data.get(&0x0499)?;
        let (temp, humidity) = Self::decode_format5(data)?;
        let mut parts = Vec::new();
        if let Some(temp) = temp {
            parts.push(format!("{temp:.1} C"));
        }
        if let Some(humidity) = humidity {
            parts.push(format!("{humidity:.1}%"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" "))
        }
    }

    fn details(&self, device: &DeviceInfo) -> Vec<DetailItem> {
        let data = match device.manufacturer_data.get(&0x0499) {
            Some(data) => data,
            None => return Vec::new(),
        };
        let (temp, humidity) = match Self::decode_format5(data) {
            Some(values) => values,
            None => return Vec::new(),
        };

        let mut details = Vec::new();
        if let Some(temp) = temp {
            details.push(DetailItem {
                label: "Ruuvi temperature".to_string(),
                value: format!("{temp:.1} C"),
            });
        }
        if let Some(humidity) = humidity {
            details.push(DetailItem {
                label: "Ruuvi humidity".to_string(),
                value: format!("{humidity:.1}%"),
            });
        }
        details
    }
}
