//! Online host identity. Guests and accounts share hosting, never account management.
use super::device_session::DeviceHandle;
use crate::account::{
    api::{ApiEnvelope, NrdApi, RoomSession},
    client::AuthenticatedClient,
};
use crate::features::host;
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct HostClient {
    pub host: host::Handle,
    identity: Identity,
}
#[derive(Clone)]
enum Identity {
    Account(Arc<AuthenticatedClient>),
    Guest(Arc<Guest>),
}
struct Guest {
    device: DeviceHandle,
    generation: String,
    api: tokio::sync::Mutex<Option<NrdApi>>,
    ended: CancellationToken,
}
impl Drop for Guest {
    fn drop(&mut self) {
        self.ended.cancel();
    }
}
impl From<Arc<AuthenticatedClient>> for HostClient {
    fn from(client: Arc<AuthenticatedClient>) -> Self {
        Self {
            host: client.host.clone(),
            identity: Identity::Account(client),
        }
    }
}
impl HostClient {
    pub fn guest(device: DeviceHandle) -> Result<Self> {
        // The installation exists before its first registration returns a device ID.
        let id = device.identity().client_identity()?.client_id;
        let generation = format!("guest:{id}");
        let mut host = host::Handle::load_guest(&id);
        host.bind_account(generation.clone());
        Ok(Self {
            host,
            identity: Identity::Guest(Arc::new(Guest {
                device,
                generation,
                api: tokio::sync::Mutex::new(None),
                ended: CancellationToken::new(),
            })),
        })
    }
    pub fn is_guest(&self) -> bool {
        matches!(self.identity, Identity::Guest(_))
    }
    pub fn generation(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.account_generation(),
            Identity::Guest(c) => c.generation.clone(),
        }
    }
    pub fn matches_saved(&self, saved: &str) -> bool {
        if self.is_guest() {
            saved.is_empty()
        } else {
            self.generation() == saved
        }
    }
    pub fn device_id(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.device_id(),
            Identity::Guest(c) => {
                c.device
                    .identity()
                    .client_identity()
                    .expect("validated device")
                    .device_id
            }
        }
    }
    pub fn presence_key(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.presence_key(),
            Identity::Guest(c) => {
                c.device
                    .identity()
                    .client_identity()
                    .expect("validated installation")
                    .client_id
            }
        }
    }
    pub fn ended(&self) -> CancellationToken {
        match &self.identity {
            Identity::Account(c) => c.ended(),
            Identity::Guest(c) => c.ended.clone(),
        }
    }
    pub fn is_active(&self) -> bool {
        !self.ended().is_cancelled()
    }
    pub fn retire(&self) {
        self.host.retire();
        match &self.identity {
            Identity::Account(c) => c.retire(),
            Identity::Guest(c) => c.ended.cancel(),
        }
    }
    pub fn clear_saved_generation(&self) -> Result<()> {
        match &self.identity {
            Identity::Account(c) => c.clear_saved_generation(),
            Identity::Guest(_) => Ok(()),
        }
    }
    pub async fn close(&self) {
        match &self.identity {
            Identity::Account(c) => c.close().await,
            Identity::Guest(c) => {
                c.ended.cancel();
                c.api.lock().await.take();
            }
        }
    }
    async fn request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        let Identity::Guest(guest) = &self.identity else {
            let Identity::Account(account) = &self.identity else {
                unreachable!()
            };
            let result = account.request(request).await;
            if result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<crate::account::api::ApiFailure>())
                .is_some_and(|e| e.code == 1120)
            {
                account.retire();
            }
            return result;
        };
        let operation = async {
            let mut active = guest.api.lock().await;
            if active.is_none() {
                let identity = guest.device.ensure(false).await?;
                let mut api = NrdApi::new(identity.client_identity()?)?;
                let credentials = api.create_guest().await?;
                api.authenticate_guest(credentials)?;
                *active = Some(api);
            }
            let api = active.as_ref().unwrap().clone();
            drop(active);
            let response = request(api).await?;
            if response.code == 1120 {
                self.retire();
            }
            response.into_data()
        };
        tokio::select! { biased; _ = guest.ended.cancelled() => bail!("游客协助已结束"), result = operation => result }
    }
    pub async fn create_host_room(&self, interval: i64) -> Result<RoomSession> {
        let guest = self.is_guest();
        let room: RoomSession = self
            .request(|api| async move {
                if guest {
                    api.create_guest_room(interval).await
                } else {
                    api.create_room(interval).await
                }
            })
            .await?;
        room.validate()?;
        Ok(room)
    }
    pub async fn set_controllable(&self, allowed: bool) -> Result<()> {
        match &self.identity {
            Identity::Account(c) => c.set_controllable(allowed).await,
            Identity::Guest(c) => {
                self.request(|api| async move { api.set_controllable(allowed).await })
                    .await?;
                c.device.set_controllable(allowed).await
            }
        }
    }
    pub async fn host_assist_identity(&self) -> Result<String> {
        let value = self
            .request(|api| async move { api.host_assist_identity().await })
            .await?;
        crate::account::assist::normalize_connect_id(&value.connect_id)
    }
    pub async fn host_assist_mode(
        &self,
        id: &str,
        allowed: bool,
        mode: &'static str,
    ) -> Result<()> {
        self.request(|api| async move { api.host_assist_mode(id, allowed, mode).await })
            .await?;
        Ok(())
    }
    pub async fn host_assist_sign(
        &self,
        id: &str,
        allowed: bool,
        sign: &str,
        backup: Option<&str>,
        mode: &'static str,
        confirm: bool,
    ) -> Result<()> {
        self.request(|api| async move {
            api.host_assist_sign(id, allowed, sign, backup, mode, confirm)
                .await
        })
        .await?;
        Ok(())
    }
    pub async fn host_assist_confirm(&self, id: &str, allowed: bool, password: bool) -> Result<()> {
        self.request(|api| async move { api.host_assist_confirm(id, allowed, password).await })
            .await?;
        Ok(())
    }
    pub async fn host_input_configuration(&self) -> Result<host::input::config::Configuration> {
        let configs = self
            .request(|api| async move {
                api.query_configures(&[
                    ("app_white_list".into(), String::new()),
                    ("win_keylock_optimize".into(), String::new()),
                ])
                .await
            })
            .await?;
        host::input::config::Configuration::from_response(configs)
    }
}
