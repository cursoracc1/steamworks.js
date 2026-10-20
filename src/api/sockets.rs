//! ISteamNetworkingSockets P2P over raw handles.
//!
//! Connections and listen sockets are plain u32 handles owned by JavaScript:
//! nothing closes them implicitly (no GC- or Drop-driven disconnects). Every
//! handle created or received here is tracked and closed by `shutdown`.
//! Accepted and outgoing connections share one poll group so a single
//! `receiveMessages` call drains all inbound traffic without running the
//! callback pump.

use crate::events::{cstr, id, push};
use napi_derive::napi;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::mem::MaybeUninit;
use std::sync::Mutex;
use steamworks::sys;

#[derive(Default)]
struct State {
    listen_sockets: HashSet<u32>,
    connections: HashSet<u32>,
    poll_group: u32,
    received_messages: u64,
    released_messages: u64,
    received_bytes: u64,
}

lazy_static! {
    static ref STATE: Mutex<State> = Mutex::new(State::default());
}

fn sockets_api() -> *mut sys::ISteamNetworkingSockets {
    unsafe { sys::SteamAPI_SteamNetworkingSockets_SteamAPI_v013() }
}

fn utils_api() -> *mut sys::ISteamNetworkingUtils {
    unsafe { sys::SteamAPI_SteamNetworkingUtils_SteamAPI_v004() }
}

fn identity_steam_id(identity: sys::SteamNetworkingIdentity) -> u64 {
    let mut identity = identity;
    unsafe { sys::SteamAPI_SteamNetworkingIdentity_GetSteamID64(&mut identity) }
}

fn info_json(connection: u32, info: &sys::SteamNetConnectionInfo_t) -> Value {
    // The struct is packed(4); copy it and read fields by value.
    let info = *info;
    let identity = info.m_identityRemote;
    let identity_type = identity.m_eType as i32;
    let flags = info.m_nFlags;
    json!({
        "connection": connection,
        "peer": id(identity_steam_id(identity)),
        "identityType": identity_type,
        "listenSocket": ({ info.m_hListenSocket }),
        "userData": ({ info.m_nUserData }),
        "state": info.m_eState as i32,
        "endReason": ({ info.m_eEndReason }),
        "endDebug": cstr(&{ info.m_szEndDebug }),
        "description": cstr(&{ info.m_szConnectionDescription }),
        "flags": flags,
        "unauthenticated": flags & sys::k_nSteamNetworkConnectionInfoFlags_Unauthenticated != 0,
        "unencrypted": flags & sys::k_nSteamNetworkConnectionInfoFlags_Unencrypted != 0,
        "relayed": flags & sys::k_nSteamNetworkConnectionInfoFlags_Relayed != 0,
        "popRelay": ({ info.m_idPOPRelay }),
        "popRemote": ({ info.m_idPOPRemote }),
    })
}

/// Raw SteamNetConnectionStatusChangedCallback_t handler (see events.rs).
pub fn on_connection_status(status: &sys::SteamNetConnectionStatusChangedCallback_t) {
    let status = *status;
    let connection = status.m_hConn;
    let info = status.m_info;
    let listen_socket = info.m_hListenSocket;
    {
        let mut state = STATE.lock().unwrap();
        if listen_socket != 0 && state.listen_sockets.contains(&listen_socket) {
            // Incoming request: tracked so shutdown frees it even if never accepted.
            state.connections.insert(connection);
        }
    }
    let mut event = info_json(connection, &info);
    event["type"] = json!("connection-status");
    event["oldState"] = json!(status.m_eOldState as i32);
    push(event);
}

/// Closes every tracked handle. Called before the client is dropped.
pub fn shutdown_all() {
    let mut state = STATE.lock().unwrap();
    let api = sockets_api();
    let reason = std::ffi::CString::new("Shutdown").unwrap();
    for connection in state.connections.drain() {
        unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_CloseConnection(
                api,
                connection,
                0,
                reason.as_ptr(),
                false,
            );
        }
    }
    for socket in state.listen_sockets.drain() {
        unsafe { sys::SteamAPI_ISteamNetworkingSockets_CloseListenSocket(api, socket) };
    }
    if state.poll_group != 0 {
        unsafe { sys::SteamAPI_ISteamNetworkingSockets_DestroyPollGroup(api, state.poll_group) };
    }
    *state = State::default();
}

fn poll_group(state: &mut State) -> u32 {
    if state.poll_group == 0 {
        state.poll_group = unsafe { sys::SteamAPI_ISteamNetworkingSockets_CreatePollGroup(sockets_api()) };
    }
    state.poll_group
}

#[napi]
pub mod sockets {
    use super::*;
    use napi::bindgen_prelude::Buffer;
    use napi::Error;

    /// Connection options applied at creation time (inherited by accepted connections
    /// when set on a listen socket). Values use the SDK's int32 units.
    #[napi(object)]
    pub struct SocketOptions {
        /// k_ESteamNetworkingConfig_P2P_Transport_ICE_Enable: 0 = SDR only, 2147483647 = all.
        pub ice_enable: Option<i32>,
        /// k_ESteamNetworkingConfig_SendBufferSize in bytes.
        pub send_buffer_size: Option<i32>,
        /// k_ESteamNetworkingConfig_TimeoutInitial in milliseconds.
        pub timeout_initial_ms: Option<i32>,
        /// k_ESteamNetworkingConfig_TimeoutConnected in milliseconds.
        pub timeout_connected_ms: Option<i32>,
    }

    #[napi(object)]
    pub struct SendResult {
        /// Raw EResult; 1 = OK.
        pub result: i32,
        pub message_number: i64,
    }

    #[napi(object)]
    pub struct NetMessage {
        pub connection: u32,
        /// Sender SteamID from the SDK, decimal string.
        pub peer: String,
        /// Copied payload; the native message is released before return.
        pub data: Buffer,
        pub message_number: i64,
        /// SteamNetworkingUtils local clock, see `localTimestampUsec`.
        pub received_usec: i64,
        pub flags: i32,
        pub lane: u32,
    }

    fn config(options: &Option<SocketOptions>) -> Vec<sys::SteamNetworkingConfigValue_t> {
        use sys::ESteamNetworkingConfigValue as Key;
        let mut out = Vec::new();
        let Some(options) = options else { return out };
        for (key, value) in [
            (Key::k_ESteamNetworkingConfig_P2P_Transport_ICE_Enable, options.ice_enable),
            (Key::k_ESteamNetworkingConfig_SendBufferSize, options.send_buffer_size),
            (Key::k_ESteamNetworkingConfig_TimeoutInitial, options.timeout_initial_ms),
            (Key::k_ESteamNetworkingConfig_TimeoutConnected, options.timeout_connected_ms),
        ] {
            if let Some(value) = value {
                let mut entry = MaybeUninit::<sys::SteamNetworkingConfigValue_t>::zeroed();
                unsafe {
                    sys::SteamAPI_SteamNetworkingConfigValue_t_SetInt32(entry.as_mut_ptr(), key, value);
                    out.push(entry.assume_init());
                }
            }
        }
        out
    }

    #[napi]
    pub fn init_relay_network_access() -> napi::Result<()> {
        crate::client::try_get_client()?;
        unsafe { sys::SteamAPI_ISteamNetworkingUtils_InitRelayNetworkAccess(utils_api()) };
        Ok(())
    }

    /// Returns raw ESteamNetworkingAvailability.
    #[napi]
    pub fn init_authentication() -> napi::Result<i32> {
        crate::client::try_get_client()?;
        Ok(unsafe { sys::SteamAPI_ISteamNetworkingSockets_InitAuthentication(sockets_api()) } as i32)
    }

    #[napi(ts_return_type = "{ availability: number; debug: string }")]
    pub fn authentication_status() -> napi::Result<Value> {
        crate::client::try_get_client()?;
        let mut details = MaybeUninit::<sys::SteamNetAuthenticationStatus_t>::zeroed();
        let availability = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_GetAuthenticationStatus(sockets_api(), details.as_mut_ptr())
        } as i32;
        let details = unsafe { details.assume_init() };
        Ok(json!({ "availability": availability, "debug": cstr(&details.m_debugMsg) }))
    }

    #[napi(ts_return_type = "import('./steam-events').RelayStatus")]
    pub fn relay_status() -> napi::Result<Value> {
        crate::client::try_get_client()?;
        let mut status = MaybeUninit::<sys::SteamRelayNetworkStatus_t>::zeroed();
        unsafe { sys::SteamAPI_ISteamNetworkingUtils_GetRelayNetworkStatus(utils_api(), status.as_mut_ptr()) };
        let status = unsafe { status.assume_init() };
        Ok(json!({
            "availability": status.m_eAvail as i32,
            "pingMeasurementInProgress": status.m_bPingMeasurementInProgress != 0,
            "availNetworkConfig": status.m_eAvailNetworkConfig as i32,
            "availAnyRelay": status.m_eAvailAnyRelay as i32,
            "debug": cstr(&status.m_debugMsg),
        }))
    }

    #[napi]
    pub fn local_timestamp_usec() -> napi::Result<i64> {
        crate::client::try_get_client()?;
        Ok(crate::events::local_timestamp_usec())
    }

    #[napi]
    pub fn create_listen_socket_p2p(virtual_port: i32, options: Option<SocketOptions>) -> napi::Result<u32> {
        crate::client::try_get_client()?;
        let options = config(&options);
        let socket = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_CreateListenSocketP2P(
                sockets_api(),
                virtual_port,
                options.len() as _,
                options.as_ptr(),
            )
        };
        if socket == sys::k_HSteamListenSocket_Invalid {
            return Err(Error::from_reason("CreateListenSocketP2P returned an invalid handle"));
        }
        STATE.lock().unwrap().listen_sockets.insert(socket);
        Ok(socket)
    }

    /// Connections accepted on this socket are closed by Steam.
    #[napi]
    pub fn close_listen_socket(socket: u32) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        STATE.lock().unwrap().listen_sockets.remove(&socket);
        Ok(unsafe { sys::SteamAPI_ISteamNetworkingSockets_CloseListenSocket(sockets_api(), socket) })
    }

    #[napi]
    pub fn connect_p2p(steam_id: String, virtual_port: i32, options: Option<SocketOptions>) -> napi::Result<u32> {
        crate::client::try_get_client()?;
        let remote = crate::events::parse_id(&steam_id)?;
        let options = config(&options);
        let mut identity = MaybeUninit::<sys::SteamNetworkingIdentity>::zeroed();
        let connection = unsafe {
            sys::SteamAPI_SteamNetworkingIdentity_SetSteamID64(identity.as_mut_ptr(), remote);
            sys::SteamAPI_ISteamNetworkingSockets_ConnectP2P(
                sockets_api(),
                identity.as_ptr(),
                virtual_port,
                options.len() as _,
                options.as_ptr(),
            )
        };
        if connection == sys::k_HSteamNetConnection_Invalid {
            return Err(Error::from_reason("ConnectP2P returned an invalid handle"));
        }
        let mut state = STATE.lock().unwrap();
        state.connections.insert(connection);
        let group = poll_group(&mut state);
        unsafe { sys::SteamAPI_ISteamNetworkingSockets_SetConnectionPollGroup(sockets_api(), connection, group) };
        Ok(connection)
    }

    /// Returns raw EResult; on OK the connection joins the shared poll group.
    #[napi]
    pub fn accept_connection(connection: u32) -> napi::Result<i32> {
        crate::client::try_get_client()?;
        let result = unsafe { sys::SteamAPI_ISteamNetworkingSockets_AcceptConnection(sockets_api(), connection) } as i32;
        if result == sys::EResult::k_EResultOK as i32 {
            let mut state = STATE.lock().unwrap();
            state.connections.insert(connection);
            let group = poll_group(&mut state);
            unsafe { sys::SteamAPI_ISteamNetworkingSockets_SetConnectionPollGroup(sockets_api(), connection, group) };
        }
        Ok(result)
    }

    /// Closes the connection and frees the handle. Also required after the peer
    /// closed or a problem was detected. Returns false for unknown handles.
    #[napi]
    pub fn close_connection(connection: u32, reason: i32, debug: Option<String>, linger: bool) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        STATE.lock().unwrap().connections.remove(&connection);
        // The CString stays alive for the duration of the native call.
        let debug = debug.map(|text| std::ffi::CString::new(text.replace('\0', "")).unwrap());
        let pointer = debug.as_ref().map_or(std::ptr::null(), |text| text.as_ptr());
        Ok(unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_CloseConnection(sockets_api(), connection, reason, pointer, linger)
        })
    }

    /// Steam copies `data`; flags are k_nSteamNetworkingSend_* (9 = ReliableNoNagle).
    #[napi]
    pub fn send_message(connection: u32, data: Buffer, flags: i32) -> napi::Result<SendResult> {
        crate::client::try_get_client()?;
        let mut message_number = 0i64;
        let result = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_SendMessageToConnection(
                sockets_api(),
                connection,
                data.as_ptr().cast(),
                data.len() as _,
                flags,
                &mut message_number,
            )
        } as i32;
        Ok(SendResult { result, message_number })
    }

    /// Drains up to `max_messages` from the shared poll group. Each payload is
    /// copied and the native message released before returning.
    #[napi]
    pub fn receive_messages(max_messages: u32) -> napi::Result<Vec<NetMessage>> {
        crate::client::try_get_client()?;
        let mut state = STATE.lock().unwrap();
        if state.poll_group == 0 || max_messages == 0 {
            return Ok(Vec::new());
        }
        let mut pointers: Vec<*mut sys::SteamNetworkingMessage_t> = vec![std::ptr::null_mut(); max_messages as usize];
        let count = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_ReceiveMessagesOnPollGroup(
                sockets_api(),
                state.poll_group,
                pointers.as_mut_ptr(),
                max_messages as _,
            )
        };
        let mut out = Vec::with_capacity(count.max(0) as usize);
        for &pointer in pointers.iter().take(count.max(0) as usize) {
            let message = unsafe { &*pointer };
            let size = message.m_cbSize.max(0) as usize;
            let data = unsafe { std::slice::from_raw_parts(message.m_pData as *const u8, size) }.to_vec();
            state.received_messages += 1;
            state.received_bytes += size as u64;
            out.push(NetMessage {
                connection: message.m_conn,
                peer: id(identity_steam_id(message.m_identityPeer)),
                data: data.into(),
                message_number: message.m_nMessageNumber,
                received_usec: message.m_usecTimeReceived,
                flags: message.m_nFlags,
                lane: message.m_idxLane as u32,
            });
            unsafe { sys::SteamAPI_SteamNetworkingMessage_t_Release(pointer) };
            state.released_messages += 1;
        }
        Ok(out)
    }

    #[napi(ts_return_type = "import('./steam-events').ConnectionInfo | null")]
    pub fn connection_info(connection: u32) -> napi::Result<Option<Value>> {
        crate::client::try_get_client()?;
        let mut info = MaybeUninit::<sys::SteamNetConnectionInfo_t>::zeroed();
        let ok = unsafe { sys::SteamAPI_ISteamNetworkingSockets_GetConnectionInfo(sockets_api(), connection, info.as_mut_ptr()) };
        Ok(ok.then(|| info_json(connection, &unsafe { info.assume_init() })))
    }

    #[napi(ts_return_type = "import('./steam-events').RealtimeStatus | null")]
    pub fn realtime_status(connection: u32) -> napi::Result<Option<Value>> {
        crate::client::try_get_client()?;
        let mut status = MaybeUninit::<sys::SteamNetConnectionRealTimeStatus_t>::zeroed();
        let result = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_GetConnectionRealTimeStatus(
                sockets_api(),
                connection,
                status.as_mut_ptr(),
                0,
                std::ptr::null_mut(),
            )
        };
        if result != sys::EResult::k_EResultOK {
            return Ok(None);
        }
        let s = unsafe { status.assume_init() };
        Ok(Some(json!({
            "state": s.m_eState as i32,
            "ping": ({ s.m_nPing }),
            "qualityLocal": ({ s.m_flConnectionQualityLocal }),
            "qualityRemote": ({ s.m_flConnectionQualityRemote }),
            "outPacketsPerSec": ({ s.m_flOutPacketsPerSec }),
            "outBytesPerSec": ({ s.m_flOutBytesPerSec }),
            "inPacketsPerSec": ({ s.m_flInPacketsPerSec }),
            "inBytesPerSec": ({ s.m_flInBytesPerSec }),
            "sendRateBytesPerSecond": ({ s.m_nSendRateBytesPerSecond }),
            "pendingUnreliable": ({ s.m_cbPendingUnreliable }),
            "pendingReliable": ({ s.m_cbPendingReliable }),
            "sentUnackedReliable": ({ s.m_cbSentUnackedReliable }),
            "queueTimeUsec": ({ s.m_usecQueueTime }),
            "maxJitterUsec": ({ s.m_usecMaxJitter }),
        })))
    }

    #[napi]
    pub fn detailed_status(connection: u32) -> napi::Result<Option<String>> {
        crate::client::try_get_client()?;
        let mut buffer = vec![0 as std::os::raw::c_char; 16384];
        let result = unsafe {
            sys::SteamAPI_ISteamNetworkingSockets_GetDetailedConnectionStatus(
                sockets_api(),
                connection,
                buffer.as_mut_ptr(),
                buffer.len() as _,
            )
        };
        Ok((result == 0).then(|| cstr(&buffer)))
    }

    /// Tracked handles and message accounting (received must equal released).
    #[napi(ts_return_type = "import('./steam-events').SocketStats")]
    pub fn stats() -> Value {
        let state = STATE.lock().unwrap();
        json!({
            "listenSockets": state.listen_sockets.len(),
            "connections": state.connections.len(),
            "pollGroup": state.poll_group,
            "receivedMessages": state.received_messages,
            "releasedMessages": state.released_messages,
            "receivedBytes": state.received_bytes,
        })
    }
}
