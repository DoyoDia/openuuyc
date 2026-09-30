//! Authenticated host-side assistance challenges (Windows 4.41.2.2602).
use super::*;

const IDENTITY: Contract = Contract {
    path: "/api/v1/device/share/info/refresh",
    method: Method::Post,
    ..DEVICE_LIST
};
const MODE: Contract = Contract {
    path: "/api/v2/room/share/upload_control_mode",
    json_content_type: true,
    ..IDENTITY
};
const SIGN: Contract = Contract {
    path: "/api/v2/room/share/upload_sign",
    ..MODE
};
const CONFIRM: Contract = Contract {
    path: "/api/v2/room/share/confirmation",
    ..MODE
};
const GUEST_IDENTITY: Contract = Contract {
    path: "/api/v1/guest/share/info",
    ..IDENTITY
};
const GUEST_MODE: Contract = Contract {
    path: "/api/v1/guest/room/share/upload_control_mode",
    ..MODE
};
const GUEST_SIGN: Contract = Contract {
    path: "/api/v1/guest/room/share/upload/sign",
    ..SIGN
};
const GUEST_CONFIRM: Contract = Contract {
    path: "/api/v1/guest/room/share/confirmation",
    ..CONFIRM
};

pub(super) fn is_sensitive(contract: Contract) -> bool {
    matches!(
        contract,
        IDENTITY | MODE | SIGN | CONFIRM | GUEST_IDENTITY | GUEST_MODE | GUEST_SIGN | GUEST_CONFIRM
    )
}

#[derive(Deserialize)]
pub(crate) struct AssistIdentity {
    pub connect_id: String,
}

impl NrdApi {
    fn host_assist_contract(&self, contract: Contract) -> Contract {
        if self.guest_id.is_none() {
            return contract;
        }
        match contract {
            IDENTITY => GUEST_IDENTITY,
            MODE => GUEST_MODE,
            SIGN => GUEST_SIGN,
            CONFIRM => GUEST_CONFIRM,
            _ => unreachable!("host assistance contract"),
        }
    }
    pub(crate) async fn host_assist_identity(&self) -> Result<ApiEnvelope<AssistIdentity>> {
        let contract = self.host_assist_contract(IDENTITY);
        self.send_with_policy(contract, contract.path, Vec::new(), true)
            .await
            .map(sanitize)
    }

    async fn host_assist_reply(
        &self,
        contract: Contract,
        body: serde_json::Value,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        let contract = self.host_assist_contract(contract);
        let mut reply: ApiEnvelope<serde_json::Value> = self
            .send_with_policy(contract, contract.path, serde_json::to_vec(&body)?, true)
            .await?;
        if reply.code == 0 && reply.data.is_none() {
            reply.data = Some(serde_json::Value::Null);
        }
        Ok(sanitize(reply))
    }

    pub(crate) async fn host_assist_mode(
        &self,
        id: &str,
        allowed: bool,
        mode: &str,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        self.host_assist_reply(
            MODE,
            serde_json::json!({"control_id":id,"allow_control":allowed,"control_mode":mode}),
        )
        .await
    }

    pub(crate) async fn host_assist_sign(
        &self,
        id: &str,
        allowed: bool,
        sign: &str,
        backup: Option<&str>,
        mode: &str,
        confirmation: bool,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        let mut body = serde_json::json!({
            "control_id":id,"can_remote_control":allowed,"sign":sign,
            "control_mode":mode,"need_confirmation":confirmation
        });
        if let Some(backup) = backup {
            body["backup_sign"] = backup.into();
        }
        self.host_assist_reply(SIGN, body).await
    }

    pub(crate) async fn host_assist_confirm(
        &self,
        id: &str,
        allowed: bool,
        password: bool,
    ) -> Result<ApiEnvelope<serde_json::Value>> {
        self.host_assist_reply(
            CONFIRM,
            serde_json::json!({
                "control_id":id,"allow_control":allowed,"need_password":password
            }),
        )
        .await
    }
}

fn sanitize<T>(mut response: ApiEnvelope<T>) -> ApiEnvelope<T> {
    // The server may echo a challenge or verifier. Keep neither in errors/UI.
    if response.code != 0 {
        response.msg = format!("远程协助请求失败（{}）", response.code);
    }
    response
}
