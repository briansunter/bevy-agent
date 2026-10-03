#![no_main]

use bevy_agent_remote::JsonRpcBridge;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(input) = std::str::from_utf8(data) {
        let bridge = JsonRpcBridge::default();
        let _ = bridge.prepare_request(input);
    }
});
