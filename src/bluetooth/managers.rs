use std::collections::HashMap;
use std::sync::Arc;
use crate::bluetooth::aacp::AACPManager;
use crate::bluetooth::att::ATTManager;

pub enum BluetoothManager {
    AACP(Arc<AACPManager>),
    ATT(Arc<ATTManager>),
}

pub struct DeviceManagers {
    att: Option<Arc<ATTManager>>,
    aacp: Option<Arc<AACPManager>>,
}

impl DeviceManagers {
    fn new() -> Self {
        Self { att: None, aacp: None }
    }

    fn with_aacp(aacp: AACPManager) -> Self {
        Self { att: None, aacp: Some(Arc::new(aacp)) }
    }

    fn with_att(att: ATTManager) -> Self {
        Self { att: Some(Arc::new(att)), aacp: None }
    }
}

pub struct BluetoothDevices {
    devices: HashMap<String, DeviceManagers>,
}

impl BluetoothDevices {
    fn new() -> Self {
        Self { devices: HashMap::new() }
    }

    fn add_aacp(&mut self, mac: String, manager: AACPManager) {
        self.devices
            .entry(mac)
            .or_insert_with(DeviceManagers::new)
            .aacp = Some(Arc::new(manager));
    }

    fn add_att(&mut self, mac: String, manager: ATTManager) {
        self.devices
            .entry(mac)
            .or_insert_with(DeviceManagers::new)
            .att = Some(Arc::new(manager));
    }
}
