//! Device-authenticated guest creation; no account credentials are attached.
use super::*;

const CREATE: Contract = Contract {
    path: "/api/v1/guest/create",
    ..LOGIN_BY_QR
};
const ROOM: Contract = Contract {
    path: "/api/v1/guest/room/create",
    ..ROOM_CREATE
};
pub(super) fn is_sensitive(contract: Contract) -> bool {
    contract == CREATE
}

#[derive(Deserialize)]
pub(crate) struct Credentials {
    guest_id: String,
    token: String,
}

impl NrdApi {
    pub(crate) async fn create_guest(&self) -> Result<Credentials> {
        let mut response: ApiEnvelope<Credentials> = self
            .send_with_policy(CREATE, CREATE.path, Vec::new(), true)
            .await?;
        if response.code != 0 {
            response.msg = format!("游客身份申请失败（{}）", response.code);
        }
        response.into_data()
    }

    pub(crate) fn authenticate_guest(&mut self, credentials: Credentials) -> Result<()> {
        anyhow::ensure!(!credentials.guest_id.is_empty(), "游客身份为空");
        HeaderValue::from_str(&credentials.guest_id).context("游客身份无效")?;
        self.set_user_id(None)?;
        self.set_bearer_token(Some(&credentials.token))?;
        self.guest_id = Some(credentials.guest_id);
        Ok(())
    }

    pub(crate) async fn create_guest_room(
        &self,
        interval: i64,
    ) -> Result<ApiEnvelope<RoomSession>> {
        self.post_json(
            ROOM,
            ROOM.path,
            &CreateRoomRequest {
                last_controlled_interval: interval,
            },
        )
        .await
    }
}
