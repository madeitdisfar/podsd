use std::io::Error;

pub(crate) async fn find_connected_airpods(adapter: &bluer::Adapter) -> bluer::Result<bluer::Device> {
    let target_uuid = uuid::Uuid::parse_str("74ec2172-0bad-4d01-8f77-997b2be0722a").unwrap();

    let addrs = adapter.device_addresses().await?;
    for addr in addrs {
        let device = adapter.device(addr)?;
        if device.is_connected().await.unwrap_or(false) {
            if let Ok(uuids) = device.uuids().await {
                if let Some(uuids) = uuids {
                    if uuids.iter().any(|u| *u == target_uuid) {
                        return Ok(device);
                    }
                }
            }
        }
    }
    Err(bluer::Error::from(Error::new(std::io::ErrorKind::NotFound, "No connected AirPods found")))
}