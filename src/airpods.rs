use crate::bluetooth::aacp::{AACPManager, ProximityKeyType, AACPEvent};
use crate::media_controller::MediaController;
use bluer::Address;
use log::{debug, info};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};

pub struct AirPodsDevice {
    pub mac_address: Address,
    pub aacp_manager: AACPManager,
    pub media_controller: Arc<Mutex<MediaController>>,
}

impl AirPodsDevice {
    pub async fn new(mac_address: Address) -> Self {
        info!("Creating new AirPodsDevice for {}", mac_address);
        let mut aacp_manager = AACPManager::new();
        aacp_manager.connect(mac_address).await;

        info!("Sending handshake");
        aacp_manager.send_handshake().await.expect(
            "Failed to send handshake to AirPods device",
        );

        sleep(Duration::from_millis(100)).await;

        info!("Setting feature flags");
        aacp_manager.send_set_feature_flags_packet().await.expect(
            "Failed to set feature flags",
        );

        sleep(Duration::from_millis(100)).await;

        info!("Requesting notifications");
        aacp_manager.send_notification_request().await.expect(
            "Failed to request notifications",
        );

        info!("Requesting Proximity Keys: IRK and ENC_KEY");
        aacp_manager.send_proximity_keys_request(
            vec![ProximityKeyType::Irk, ProximityKeyType::EncKey],
        ).await.expect(
            "Failed to request proximity keys",
        );
        let media_controller = Arc::new(Mutex::new(MediaController::new(mac_address.to_string())));
        let mc_clone = media_controller.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        aacp_manager.set_event_channel(tx).await;

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    AACPEvent::EarDetection(old_status, new_status) => {
                        debug!("Received EarDetection event: old_status={:?}, new_status={:?}", old_status, new_status);
                        let controller = mc_clone.lock().await;
                        debug!("Calling handle_ear_detection with old_status: {:?}, new_status: {:?}", old_status, new_status);
                        controller.handle_ear_detection(old_status, new_status).await;
                    }
                    _ => {
                        debug!("Received unhandled AACP event: {:?}", event);
                    }
                }
            }
        });

        AirPodsDevice {
            mac_address,
            aacp_manager,
            media_controller,
        }
    }
}
