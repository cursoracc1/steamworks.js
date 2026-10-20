//! Single Steam callback pump and event queue.
//!
//! `run_callbacks` (driven by index.js) is the only place that dispatches Steam
//! callbacks. Lobby, server and networking events are converted to plain JSON
//! values here and queued; JavaScript drains them with `runtime.drainEvents()`.
//! All 64-bit IDs are emitted as decimal strings.

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::raw::c_char;
use std::sync::Mutex;
use std::time::Instant;
use steamworks::{sys, Callback, CallbackHandle, CallbackResult, Client, SteamError};

/// Upper bound for undrained events. Overflow is counted in pump stats, never silent.
const MAX_QUEUED_EVENTS: usize = 65536;

struct Queue {
    events: VecDeque<Value>,
    dropped: u64,
}

#[derive(Default)]
struct Pump {
    runs: u64,
    gaps: u64,
    total_gap_us: u64,
    max_gap_us: u64,
    last_gap_us: u64,
    max_run_us: u64,
    last: Option<Instant>,
}

lazy_static! {
    static ref QUEUE: Mutex<Queue> = Mutex::new(Queue {
        events: VecDeque::new(),
        dropped: 0
    });
    static ref PUMP: Mutex<Pump> = Mutex::new(Pump::default());
    /// Raw callback registrations; dropping a handle unregisters it.
    static ref RAW_HANDLES: Mutex<Vec<CallbackHandle>> = Mutex::new(Vec::new());
}

pub fn id(value: u64) -> String {
    value.to_string()
}

pub fn parse_id(value: &str) -> napi::Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| napi::Error::from_reason(format!("Invalid 64-bit Steam ID: {value}")))
}

pub fn cstr(buffer: &[c_char]) -> String {
    let bytes: Vec<u8> = buffer
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub fn result_name<T>(result: &Result<T, SteamError>) -> String {
    match result {
        Ok(_) => "OK".to_string(),
        Err(error) => format!("{error:?}"),
    }
}

/// SteamNetworkingUtils local clock (microseconds). Same clock as
/// `SteamNetworkingMessage_t::m_usecTimeReceived`.
pub fn local_timestamp_usec() -> i64 {
    unsafe {
        sys::SteamAPI_ISteamNetworkingUtils_GetLocalTimestamp(
            sys::SteamAPI_SteamNetworkingUtils_SteamAPI_v004(),
        )
    }
}

pub fn push(mut event: Value) {
    if let Value::Object(map) = &mut event {
        map.insert("steamUsec".into(), json!(local_timestamp_usec()));
    }
    let mut queue = QUEUE.lock().unwrap();
    if queue.events.len() >= MAX_QUEUED_EVENTS {
        queue.dropped += 1;
        return;
    }
    queue.events.push_back(event);
}

pub fn drain() -> Vec<Value> {
    QUEUE.lock().unwrap().events.drain(..).collect()
}

pub fn record_pump(started: Instant) {
    let now = Instant::now();
    let mut pump = PUMP.lock().unwrap();
    if let Some(last) = pump.last {
        let gap = started.duration_since(last).as_micros() as u64;
        pump.gaps += 1;
        pump.total_gap_us += gap;
        pump.last_gap_us = gap;
        pump.max_gap_us = pump.max_gap_us.max(gap);
    }
    pump.runs += 1;
    pump.max_run_us = pump.max_run_us.max(now.duration_since(started).as_micros() as u64);
    pump.last = Some(started);
}

pub fn pump_stats(reset: bool) -> Value {
    let mut pump = PUMP.lock().unwrap();
    let queue = QUEUE.lock().unwrap();
    let stats = json!({
        "runs": pump.runs,
        "gaps": pump.gaps,
        "meanGapUs": if pump.gaps > 0 { pump.total_gap_us / pump.gaps } else { 0 },
        "maxGapUs": pump.max_gap_us,
        "lastGapUs": pump.last_gap_us,
        "maxRunUs": pump.max_run_us,
        "queuedEvents": queue.events.len(),
        "droppedEvents": queue.dropped,
    });
    if reset {
        let last = pump.last;
        *pump = Pump {
            last,
            ..Pump::default()
        };
    }
    stats
}

/// Handler passed to `Client::process_callbacks`.
pub fn on_callback(callback: CallbackResult) {
    let event = match callback {
        CallbackResult::LobbyChatUpdate(v) => json!({
            "type": "lobby-chat-update",
            "lobby": id(v.lobby.raw()),
            "userChanged": id(v.user_changed.raw()),
            "makingChange": id(v.making_change.raw()),
            "change": v.member_state_change.bits(),
        }),
        CallbackResult::LobbyDataUpdate(v) => json!({
            "type": "lobby-data-update",
            "lobby": id(v.lobby.raw()),
            "member": id(v.member.raw()),
            "success": v.success,
        }),
        CallbackResult::LobbyChatMsg(v) => {
            let mut sender = sys::CSteamID {
                m_steamid: sys::CSteamID_SteamID_t { m_unAll64Bits: 0 },
            };
            let mut entry_type: i32 = 0;
            let mut buffer = vec![0u8; 4096];
            let size = unsafe {
                sys::SteamAPI_ISteamMatchmaking_GetLobbyChatEntry(
                    sys::SteamAPI_SteamMatchmaking_v009(),
                    v.lobby.raw(),
                    v.chat_id,
                    &mut sender,
                    buffer.as_mut_ptr().cast(),
                    buffer.len() as _,
                    (&mut entry_type as *mut i32).cast(),
                )
            };
            buffer.truncate(size.max(0) as usize);
            if buffer.last() == Some(&0) {
                buffer.pop();
            }
            let entry_sender = unsafe { sender.m_steamid.m_unAll64Bits };
            json!({
                "type": "lobby-chat-msg",
                "lobby": id(v.lobby.raw()),
                "sender": id(v.user.raw()),
                "entrySender": id(entry_sender),
                "entryType": entry_type,
                "chatId": v.chat_id,
                "text": String::from_utf8_lossy(&buffer),
            })
        }
        CallbackResult::LobbyCreated(v) => json!({
            "type": "lobby-created",
            "lobby": id(v.lobby.raw()),
            "result": result_name(&v.result),
        }),
        CallbackResult::SteamServersConnected(_) => json!({ "type": "steam-servers-connected" }),
        CallbackResult::SteamServersDisconnected(v) => json!({
            "type": "steam-servers-disconnected",
            "reason": result_name(&v.reason),
        }),
        CallbackResult::SteamServerConnectFailure(v) => json!({
            "type": "steam-server-connect-failure",
            "reason": result_name(&v.reason),
            "stillRetrying": v.still_retrying,
        }),
        _ => return,
    };
    push(event);
}

// Callbacks whose high-level conversion in steamworks-rs drops fields we need
// (raw enter response, connection handle, end reason). Registered by ID; no
// other code in this binding registers these IDs.
macro_rules! raw_callback {
    ($name:ident, $sys:ident) => {
        struct $name(sys::$sys);
        unsafe impl Callback for $name {
            const ID: i32 = callback_id!($sys);
            unsafe fn from_raw(raw: *mut c_void) -> Self {
                Self(raw.cast::<sys::$sys>().read_unaligned())
            }
        }
    };
}
macro_rules! callback_id {
    (LobbyEnter_t) => {
        sys::LobbyEnter_t_k_iCallback as i32
    };
    (SteamNetConnectionStatusChangedCallback_t) => {
        sys::SteamNetConnectionStatusChangedCallback_t_k_iCallback as i32
    };
    (SteamNetAuthenticationStatus_t) => {
        sys::SteamNetAuthenticationStatus_t_k_iCallback as i32
    };
    (SteamRelayNetworkStatus_t) => {
        sys::SteamRelayNetworkStatus_t_k_iCallback as i32
    };
}
raw_callback!(RawLobbyEnter, LobbyEnter_t);
raw_callback!(RawConnectionStatus, SteamNetConnectionStatusChangedCallback_t);
raw_callback!(RawAuthenticationStatus, SteamNetAuthenticationStatus_t);
raw_callback!(RawRelayStatus, SteamRelayNetworkStatus_t);

pub fn register_raw(client: &Client) {
    let mut handles = RAW_HANDLES.lock().unwrap();
    handles.clear();
    handles.push(client.register_callback(|v: RawLobbyEnter| {
        let v = v.0;
        push(json!({
            "type": "lobby-enter",
            "lobby": id({ v.m_ulSteamIDLobby }),
            "response": ({ v.m_EChatRoomEnterResponse }),
            "locked": ({ v.m_bLocked }),
        }));
    }));
    handles.push(client.register_callback(|v: RawConnectionStatus| {
        crate::api::sockets::on_connection_status(&v.0);
    }));
    handles.push(client.register_callback(|v: RawAuthenticationStatus| {
        let v = v.0;
        push(json!({
            "type": "authentication-status",
            "availability": v.m_eAvail as i32,
            "debug": cstr(&v.m_debugMsg),
        }));
    }));
    handles.push(client.register_callback(|v: RawRelayStatus| {
        let v = v.0;
        push(json!({
            "type": "relay-status",
            "availability": v.m_eAvail as i32,
            "pingMeasurementInProgress": v.m_bPingMeasurementInProgress != 0,
            "availNetworkConfig": v.m_eAvailNetworkConfig as i32,
            "availAnyRelay": v.m_eAvailAnyRelay as i32,
            "debug": cstr(&v.m_debugMsg),
        }));
    }));
}

/// Unregisters raw callbacks and clears queued events (shutdown).
pub fn clear() {
    RAW_HANDLES.lock().unwrap().clear();
    let mut queue = QUEUE.lock().unwrap();
    queue.events.clear();
    queue.dropped = 0;
}
