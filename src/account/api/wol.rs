//! S 4.41 B2DBA0: authenticated IPv4 LAN registration; no router object unless discovered.
use super::*;
use crate::features::host::wol::packet::LanInfo;
const INFO: Contract = Contract {
    method: Method::Post,
    path: "/api/v1/device/wake_on_lan/info",
    json_content_type: true,
    ..DEVICE_LIST
};
#[derive(serde::Deserialize)]
pub(crate) struct Registered {
    pub support_wol: bool,
}
impl NrdApi {
    pub(crate) async fn set_wol(&self, enabled: bool) -> Result<ApiEnvelope<()>> {
        let contract = Contract {
            path: "/api/v1/device/set_support_wol",
            ..INFO
        };
        let response: ApiEnvelope<serde_json::Value> = self
            .send_with_policy(
                contract,
                contract.path,
                serde_json::to_vec(&serde_json::json!({"support_wol":enabled}))?,
                true,
            )
            .await?;
        Ok(ApiEnvelope {
            code: response.code,
            msg: response.msg,
            data: Some(()),
        })
    }
    pub(crate) async fn wol_info(&self, info: LanInfo) -> Result<ApiEnvelope<Registered>> {
        let mut api = self.clone();
        api.http = self.http.ipv4_route();
        api.send_with_policy(INFO, INFO.path, serde_json::to_vec(&info)?, true)
            .await
    }
}
