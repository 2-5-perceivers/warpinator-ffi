pub mod config;
#[cfg(feature = "power_manager")]
pub mod power_manager;
#[cfg(any(feature = "tracing_callbacks", feature = "tracing_android"))]
pub mod tracing;
pub mod types;

use crate::config::{ProtocolConfig, UserConfig};
use crate::types::message::Message;
use crate::types::remote::Remote;
use crate::types::transfer::Transfer;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use warpinator_lib::remote_manager::WarpEvent;

uniffi::setup_scaffolding!("warpinator");

#[derive(uniffi::Object)]
pub struct Warpinator {
    server: Mutex<Option<warpinator_lib::WarpinatorServer>>,
    remote_manager: RwLock<Option<warpinator_lib::remote_manager::RemoteManager>>,
    shutdown_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    runtime: tokio::runtime::Runtime,
}

#[derive(uniffi::Error, thiserror::Error, Debug)]
pub enum WarpError {
    #[error("Runtime error")]
    RuntimeError,
    #[error("Invalid IP address")]
    InvalidIp,
    #[error("Build server error")]
    BuildServerError(String),
    #[error("Already started")]
    AlreadyStarted,
    #[error("Not found")]
    NotFound,
}

type Result<T> = std::result::Result<T, WarpError>;

#[uniffi::export(callback_interface)]
pub trait WarpEventListener: Send + Sync {
    fn on_remote_added(&self, uuid: String);
    fn on_remote_updated(&self, uuid: String);
    fn on_transfer_added(&self, remote_uuid: String, transfer_uuid: String);
    fn on_transfer_updated(&self, remote_uuid: String, transfer_uuid: String);
    fn on_transfer_removed(&self, remote_uuid: String, transfer_uuid: String);
    fn on_message_added(&self, remote_uuid: String, message_uuid: String);
    fn on_message_removed(&self, remote_uuid: String, message_uuid: String);
}

#[cfg(not(feature = "power_manager"))]
#[uniffi::export]
impl Warpinator {
    #[uniffi::constructor]
    pub fn new(
        config: UserConfig,
        protocol_config: Option<ProtocolConfig>,
        service_name: String,
    ) -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Runtime::new().map_err(|_| WarpError::RuntimeError)?;
        let user_config = config.to_config()?;
        let protocol_config = protocol_config.map(|c| c.to_config());

        let mut server_builder =
            warpinator_lib::WarpinatorServer::builder().user_config(user_config);

        if let Some(protocol_config) = protocol_config {
            server_builder = server_builder.protocol_config(protocol_config);
        }

        let server = server_builder
            .service_name(service_name.as_str())
            .build()
            .map_err(|e| WarpError::BuildServerError(e.to_string()))?;

        let remote_manager = server.remotes.clone();

        Ok(Arc::new(Self {
            server: Mutex::new(Some(server)),
            remote_manager: RwLock::new(Some(remote_manager)),
            shutdown_tx: Mutex::new(None),
            runtime,
        }))
    }
}

#[cfg(feature = "power_manager")]
#[uniffi::export]
impl Warpinator {
    #[uniffi::constructor]
    pub fn new(
        config: UserConfig,
        protocol_config: Option<ProtocolConfig>,
        service_name: String,
        power_manager: Box<dyn power_manager::PowerManager>,
    ) -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Runtime::new().map_err(|_| WarpError::RuntimeError)?;
        let user_config = config.to_config()?;
        let protocol_config = protocol_config.map(|c| c.to_config());

        let mut server_builder =
            warpinator_lib::WarpinatorServer::builder().user_config(user_config);

        if let Some(protocol_config) = protocol_config {
            server_builder = server_builder.protocol_config(protocol_config);
        }
        server_builder = server_builder.power_manager(Arc::new(
            power_manager::PowerManagerWrapper::new(power_manager),
        ));

        let server = server_builder
            .service_name(service_name.as_str())
            .build()
            .map_err(|e| WarpError::BuildServerError(e.to_string()))?;

        let remote_manager = server.remotes.clone();

        Ok(Arc::new(Self {
            server: Mutex::new(Some(server)),
            remote_manager: RwLock::new(Some(remote_manager)),
            shutdown_tx: Mutex::new(None),
            runtime,
        }))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Warpinator {
    pub fn start(&self, listener: Box<dyn WarpEventListener>) -> Result<()> {
        let server = self
            .server
            .lock()
            .unwrap()
            .take()
            .ok_or(WarpError::AlreadyStarted)?;

        let remote_manager = self
            .remote_manager
            .read()
            .map_err(|_| WarpError::RuntimeError)?
            .as_ref()
            .cloned()
            .ok_or(WarpError::AlreadyStarted)?;

        let (tx, rx) = tokio::sync::oneshot::channel();
        *self.shutdown_tx.lock().unwrap() = Some(tx);

        self.runtime.spawn(async move {
            let mut events = remote_manager.subscribe();
            while let Ok(event) = events.recv().await {
                match event {
                    WarpEvent::RemoteAdded(uuid) => listener.on_remote_added(uuid),
                    WarpEvent::RemoteUpdated(uuid) => listener.on_remote_updated(uuid),
                    WarpEvent::TransferAdded(r, t) => listener.on_transfer_added(r, t),
                    WarpEvent::TransferUpdated(r, t) => listener.on_transfer_updated(r, t),
                    WarpEvent::TransferRemoved(r, t) => listener.on_transfer_removed(r, t),
                    #[cfg(feature = "messaging")]
                    WarpEvent::MessageAdded(r, m) => listener.on_message_added(r, m),
                    #[cfg(feature = "messaging")]
                    WarpEvent::MessageRemoved(r, m) => listener.on_message_removed(r, m),
                    _ => {}
                }
            }
        });

        self.runtime.spawn(async move {
            let _ = server
                .serve_with_shutdown(async {
                    rx.await.ok();
                })
                .await;
        });

        Ok(())
    }

    pub fn stop(&self) {
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }

        self.remote_manager.write().unwrap().take();
    }

    pub async fn manual_connection(&self, url: &str) -> Result<()> {
        self.manager()?
            .manual_connection(url)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn remove_transfer(&self, remote_uuid: &str, transfer_uuid: &str) -> Result<()> {
        self.manager()?
            .remove_transfer(remote_uuid, transfer_uuid)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    #[cfg(feature = "messaging")]
    pub async fn remove_message(&self, remote_uuid: &str, message_uuid: &str) -> Result<()> {
        self.manager()?
            .remove_message(remote_uuid, message_uuid)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn connect_remote(&self, uuid: &str) -> Result<()> {
        self.manager()?
            .get_worker(uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .connect()
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn send_transfer_request(&self, remote_uuid: &str, paths: Vec<String>) -> Result<()> {
        let paths: Vec<PathBuf> = paths.into_iter().map(|p| p.into()).collect();
        self.manager()?
            .get_worker(remote_uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .send_transfer_request(paths)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    #[cfg(feature = "messaging")]
    pub async fn send_message(&self, remote_uuid: &str, content: String) -> Result<()> {
        self.manager()?
            .get_worker(remote_uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .send_message(content.as_str())
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn accept_transfer(
        &self,
        remote_uuid: &str,
        transfer_uuid: &str,
        path: String,
    ) -> Result<()> {
        self.manager()?
            .get_worker(remote_uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .accept_transfer::<PathBuf>(transfer_uuid, path.into())
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn stop_transfer(
        &self,
        remote_uuid: &str,
        transfer_uuid: &str,
        error: bool,
    ) -> Result<()> {
        self.manager()?
            .get_worker(remote_uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .stop_transfer(transfer_uuid, error)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn cancel_transfer(&self, remote_uuid: &str, transfer_uuid: &str) -> Result<()> {
        self.manager()?
            .get_worker(remote_uuid)
            .await
            .ok_or(WarpError::RuntimeError)?
            .cancel_transfer(transfer_uuid)
            .await
            .map_err(|_| WarpError::RuntimeError)
    }

    pub async fn remote(&self, uuid: &str) -> Result<Remote> {
        self.manager()?
            .remote(uuid)
            .await
            .ok_or(WarpError::NotFound)
            .map(|r| Remote::from(&r))
    }

    pub async fn remotes(&self) -> Result<Vec<Remote>> {
        Ok(self
            .manager()?
            .remotes()
            .await
            .iter()
            .map(Remote::from)
            .collect())
    }

    pub async fn transfer(&self, remote_uuid: &str, transfer_uuid: &str) -> Result<Transfer> {
        self.manager()?
            .transfer(remote_uuid, transfer_uuid)
            .await
            .ok_or(WarpError::NotFound)
            .map(|t| Transfer::from(&t))
    }

    pub async fn transfers(&self, remote_uuid: &str) -> Result<Vec<Transfer>> {
        Ok(self
            .manager()?
            .transfers(remote_uuid)
            .await
            .ok_or(WarpError::NotFound)?
            .iter()
            .map(Transfer::from)
            .collect())
    }

    #[cfg(feature = "messaging")]
    pub async fn message(&self, remote_uuid: &str, message_uuid: &str) -> Result<Message> {
        self.manager()?
            .message(remote_uuid, message_uuid)
            .await
            .ok_or(WarpError::NotFound)
            .map(|m| Message::from(&m))
    }

    #[cfg(feature = "messaging")]
    pub async fn messages(&self, remote_uuid: &str) -> Result<Vec<Message>> {
        Ok(self
            .manager()?
            .messages(remote_uuid)
            .await
            .ok_or(WarpError::NotFound)?
            .iter()
            .map(Message::from)
            .collect())
    }
}

impl Warpinator {
    fn manager(&self) -> Result<warpinator_lib::remote_manager::RemoteManager> {
        self.remote_manager
            .read()
            .map_err(|_| WarpError::RuntimeError)?
            .clone()
            .ok_or(WarpError::RuntimeError)
    }
}

impl Drop for Warpinator {
    fn drop(&mut self) {
        self.stop();
    }
}
