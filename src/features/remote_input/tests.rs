use super::*;
use serde_json::{Value, json};

const OWNER: u64 = 7;

fn active_input(platform: i32) -> RemoteInput {
    let input = RemoteInput::default();
    input.set_keyboard_platform(platform);
    input.set_ready(true);
    input.enable(MouseMode::Smart, false).unwrap();
    input.confirm_neutral(input.activation_generation());
    input
}

async fn submitted(input: &RemoteInput) -> Value {
    let work = tokio::time::timeout(Duration::from_secs(1), input.next())
        .await
        .expect("accepted keyboard event must reach the transport queue");
    let InputWork::Event(event) = work else {
        panic!("unexpected input recovery");
    };
    let value = serde_json::from_slice(&event.event.encode()).unwrap();
    input.complete(&event, Ok(()));
    value
}

#[tokio::test]
async fn mac_a_reaches_transport_as_zero_and_releases() {
    let input = active_input(4);
    assert!(input.keyboard_supported());
    assert!(input.key(OWNER, 0x41, true, None));
    assert!(input.owner_holds_key(OWNER, 0x41));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":0})
    );
    assert!(input.key(OWNER, 0x41, false, None));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_release","key":0})
    );
    assert!(!input.owner_holds_key(OWNER, 0x41));
    assert!(input.lock().remote_keys.is_empty());
}

#[tokio::test]
async fn focus_loss_releases_mac_ordinary_keys_before_modifiers() {
    let input = active_input(4);
    for (vk, code) in [(0x5b, 0x37), (0xa0, 0x38), (0x41, 0x00)] {
        assert!(input.key(OWNER, vk, true, None));
        assert_eq!(
            submitted(&input).await,
            json!({"action":"kbd_press","key":code})
        );
    }
    input.pause_owner(OWNER);
    for code in [0x00, 0x37, 0x38] {
        assert_eq!(
            submitted(&input).await,
            json!({"action":"kbd_release","key":code})
        );
    }
    assert!(input.lock().remote_keys.is_empty());
    input.reconcile_keyboard_releases();
    for code in [0x00, 0x37, 0x38] {
        assert_eq!(
            submitted(&input).await,
            json!({"action":"kbd_release","key":code})
        );
    }
    assert!(input.lock().queue.is_empty());
}

#[tokio::test]
async fn recovery_releases_in_flight_mac_keys_with_native_codes() {
    let input = active_input(4);
    assert!(input.key(OWNER, 0xa2, true, None));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":0x3b})
    );
    assert!(input.key(OWNER, 0x41, true, None));
    let InputWork::Event(in_flight) = input.next().await else {
        panic!("expected in-flight A press");
    };
    let recovery = input.pause_transport(&in_flight).unwrap();
    let releases: Vec<Value> = recovery
        .releases
        .iter()
        .map(|event| serde_json::from_slice(&event.encode()).unwrap())
        .collect();
    assert_eq!(
        releases,
        vec![
            json!({"action":"kbd_release","key":0}),
            json!({"action":"kbd_release","key":0x3b})
        ]
    );
    assert!(!input.is_current(&in_flight));
    input.finish_transport_recovery(recovery.epoch, Ok(()));
    assert!(input.lock().remote_keys.is_empty());
}

#[tokio::test]
async fn mac_caps_lock_repeats_do_not_toggle_again_and_up_is_preserved() {
    let input = active_input(4);
    assert!(input.key(OWNER, 0x14, true, Some(128)));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":0x39})
    );
    assert!(input.key(OWNER, 0x14, true, Some(128)));
    assert!(input.lock().queue.is_empty());
    assert!(input.key(OWNER, 0x14, false, Some(128)));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_release","key":0x39})
    );
}

#[tokio::test]
async fn windows_keeps_vk_lock_state_and_secure_attention_sequence() {
    let input = active_input(1);
    assert!(input.key(OWNER, 0x41, true, None));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":65,"interrept":"i"})
    );
    assert!(input.key(OWNER, 0x41, false, None));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_release","key":65,"interrept":"i"})
    );
    assert!(input.key(OWNER, 0x90, true, Some(0)));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":144,"interrept":"i","lockkeysstatus":0})
    );
    assert!(input.key(OWNER, 0x90, false, Some(128)));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_release","key":144,"interrept":"i","lockkeysstatus":128})
    );
    assert!(input.supports_ctrl_alt_del());
    input.send_ctrl_alt_del(OWNER).unwrap();
    for (key, down) in [
        (162, true),
        (164, true),
        (46, true),
        (46, false),
        (164, false),
        (162, false),
    ] {
        assert_eq!(
            submitted(&input).await,
            json!({"action":if down {"kbd_press"} else {"kbd_release"},"key":key})
        );
    }
}

#[tokio::test]
async fn mac_rejects_windows_only_keys_and_command_but_keeps_heartbeat() {
    let input = active_input(4);
    assert!(!input.supports_ctrl_alt_del());
    assert!(input.send_ctrl_alt_del(OWNER).is_err());
    assert!(!input.key(OWNER, 0x90, true, Some(128)));
    assert!(input.lock().queue.is_empty());
    assert!(input.key(OWNER, 0xa0, true, None));
    assert_eq!(
        submitted(&input).await,
        json!({"action":"kbd_press","key":0x38})
    );
    input.heartbeat();
    assert_eq!(
        submitted(&input).await,
        json!({"heartbeat":"kbd_heartbeat"})
    );
}
