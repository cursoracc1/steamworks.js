//! Event-driven lobby API with string IDs.
//!
//! Async operations return a numeric token immediately; the completion is
//! queued as an event carrying that token (`lobby-create-result`,
//! `lobby-join-result`, `lobby-list-result`). The raw join response arrives in
//! the separate `lobby-enter` event.

use crate::events::{id, parse_id, push};
use napi_derive::napi;
use serde_json::{json, Map, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};
use steamworks::{sys, SteamId};

static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);

fn mm() -> *mut sys::ISteamMatchmaking {
    unsafe { sys::SteamAPI_SteamMatchmaking_v009() }
}

fn c(text: &str) -> napi::Result<CString> {
    CString::new(text).map_err(|_| napi::Error::from_reason("String contains a NUL byte"))
}

fn lobby_data(lobby: u64) -> Map<String, Value> {
    let mut data = Map::new();
    let count = unsafe { sys::SteamAPI_ISteamMatchmaking_GetLobbyDataCount(mm(), lobby) };
    for index in 0..count {
        let mut key = vec![0 as c_char; 256];
        let mut value = vec![0 as c_char; 8192];
        let ok = unsafe {
            sys::SteamAPI_ISteamMatchmaking_GetLobbyDataByIndex(
                mm(),
                lobby,
                index,
                key.as_mut_ptr(),
                key.len() as _,
                value.as_mut_ptr(),
                value.len() as _,
            )
        };
        if ok {
            data.insert(crate::events::cstr(&key), json!(crate::events::cstr(&value)));
        }
    }
    data
}

/// Public metadata and counts: the only fields Steam provides for lobbies we are not in.
fn lobby_summary(lobby: u64) -> Value {
    json!({
        "id": id(lobby),
        "memberCount": unsafe { sys::SteamAPI_ISteamMatchmaking_GetNumLobbyMembers(mm(), lobby) },
        "memberLimit": unsafe { sys::SteamAPI_ISteamMatchmaking_GetLobbyMemberLimit(mm(), lobby) },
        "data": lobby_data(lobby),
    })
}

fn member_data(lobby: u64, member: u64, key: &CStr) -> Option<String> {
    let value = unsafe { sys::SteamAPI_ISteamMatchmaking_GetLobbyMemberData(mm(), lobby, member, key.as_ptr()) };
    (!value.is_null()).then(|| unsafe { CStr::from_ptr(value) }.to_string_lossy().into_owned())
}

#[napi]
pub mod lobby {
    use super::*;
    use napi::bindgen_prelude::Buffer;
    use napi::Error;

    /// `comparison` is ELobbyComparison: -2 <=, -1 <, 0 ==, 1 >, 2 >=, 3 !=.
    #[napi(object)]
    pub struct StringFilter {
        pub key: String,
        pub value: String,
        pub comparison: i32,
    }

    #[napi(object)]
    pub struct NumberFilter {
        pub key: String,
        pub value: i32,
        pub comparison: i32,
    }

    #[napi(object)]
    pub struct NearFilter {
        pub key: String,
        pub value: i32,
    }

    /// Applied together with the request in one call, so filters never leak
    /// between queries.
    #[napi(object)]
    pub struct LobbyFilter {
        pub strings: Option<Vec<StringFilter>>,
        pub numbers: Option<Vec<NumberFilter>>,
        pub near: Option<Vec<NearFilter>>,
        pub slots_available: Option<i32>,
        /// ELobbyDistanceFilter: 0 close, 1 default, 2 far, 3 worldwide.
        pub distance: Option<u32>,
        pub count: Option<i32>,
    }

    fn comparison(value: i32) -> napi::Result<sys::ELobbyComparison> {
        use sys::ELobbyComparison::*;
        Ok(match value {
            -2 => k_ELobbyComparisonEqualToOrLessThan,
            -1 => k_ELobbyComparisonLessThan,
            0 => k_ELobbyComparisonEqual,
            1 => k_ELobbyComparisonGreaterThan,
            2 => k_ELobbyComparisonEqualToOrGreaterThan,
            3 => k_ELobbyComparisonNotEqual,
            _ => return Err(Error::from_reason(format!("Invalid lobby comparison {value}"))),
        })
    }

    fn token() -> u32 {
        NEXT_TOKEN.fetch_add(1, Ordering::Relaxed)
    }

    /// `lobby_type`: 0 private, 1 friends only, 2 public, 3 invisible.
    /// Completion: `lobby-create-result`.
    #[napi]
    pub fn create(lobby_type: u32, max_members: u32) -> napi::Result<u32> {
        let client = crate::client::try_get_client()?;
        let ty = match lobby_type {
            0 => steamworks::LobbyType::Private,
            1 => steamworks::LobbyType::FriendsOnly,
            2 => steamworks::LobbyType::Public,
            3 => steamworks::LobbyType::Invisible,
            _ => return Err(Error::from_reason(format!("Invalid lobby type {lobby_type}"))),
        };
        if !(1..=250).contains(&max_members) {
            return Err(Error::from_reason("max_members must be 1-250"));
        }
        let token = token();
        client.matchmaking().create_lobby(ty, max_members, move |result| {
            push(json!({
                "type": "lobby-create-result",
                "token": token,
                "ok": result.is_ok(),
                "lobby": result.as_ref().ok().map(|lobby| id(lobby.raw())),
                "result": crate::events::result_name(&result),
            }));
        });
        Ok(token)
    }

    /// Completion: `lobby-join-result`; the raw EChatRoomEnterResponse is in `lobby-enter`.
    #[napi]
    pub fn join(lobby: String) -> napi::Result<u32> {
        let client = crate::client::try_get_client()?;
        let target = parse_id(&lobby)?;
        let token = token();
        client.matchmaking().join_lobby(steamworks::LobbyId::from_raw(target), move |result| {
            push(json!({
                "type": "lobby-join-result",
                "token": token,
                "ok": result.is_ok(),
                "lobby": id(target),
            }));
        });
        Ok(token)
    }

    /// Completion: `lobby-list-result` with public data and counts for each match.
    #[napi]
    pub fn request_list(filter: Option<LobbyFilter>) -> napi::Result<u32> {
        let client = crate::client::try_get_client()?;
        // Validate everything before touching Steam's pending filter state.
        let mut strings = Vec::new();
        let mut numbers = Vec::new();
        let mut near = Vec::new();
        let mut distance = None;
        if let Some(filter) = &filter {
            for f in filter.strings.iter().flatten() {
                strings.push((c(&f.key)?, c(&f.value)?, comparison(f.comparison)?));
            }
            for f in filter.numbers.iter().flatten() {
                numbers.push((c(&f.key)?, f.value, comparison(f.comparison)?));
            }
            for f in filter.near.iter().flatten() {
                near.push((c(&f.key)?, f.value));
            }
            distance = match filter.distance {
                None => None,
                Some(0) => Some(sys::ELobbyDistanceFilter::k_ELobbyDistanceFilterClose),
                Some(1) => Some(sys::ELobbyDistanceFilter::k_ELobbyDistanceFilterDefault),
                Some(2) => Some(sys::ELobbyDistanceFilter::k_ELobbyDistanceFilterFar),
                Some(3) => Some(sys::ELobbyDistanceFilter::k_ELobbyDistanceFilterWorldwide),
                Some(other) => return Err(Error::from_reason(format!("Invalid distance filter {other}"))),
            };
        }
        unsafe {
            for (key, value, kind) in &strings {
                sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListStringFilter(mm(), key.as_ptr(), value.as_ptr(), *kind);
            }
            for (key, value, kind) in &numbers {
                sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListNumericalFilter(mm(), key.as_ptr(), *value, *kind);
            }
            for (key, value) in &near {
                sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListNearValueFilter(mm(), key.as_ptr(), *value);
            }
            if let Some(filter) = &filter {
                if let Some(slots) = filter.slots_available {
                    sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListFilterSlotsAvailable(mm(), slots);
                }
                if let Some(distance) = distance {
                    sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListDistanceFilter(mm(), distance);
                }
                if let Some(count) = filter.count {
                    sys::SteamAPI_ISteamMatchmaking_AddRequestLobbyListResultCountFilter(mm(), count);
                }
            }
        }
        let token = token();
        client.matchmaking().request_lobby_list(move |result| {
            push(json!({
                "type": "lobby-list-result",
                "token": token,
                "ok": result.is_ok(),
                "result": crate::events::result_name(&result),
                "lobbies": result
                    .as_ref()
                    .map(|lobbies| lobbies.iter().map(|lobby| lobby_summary(lobby.raw())).collect::<Vec<_>>())
                    .unwrap_or_default(),
            }));
        });
        Ok(token)
    }

    /// Snapshot of a lobby. Owner and members are only valid for lobbies we are in;
    /// `member_keys` selects which member data keys to read (Steam cannot enumerate them).
    #[napi(ts_return_type = "import('./steam-events').LobbySnapshot")]
    pub fn get(lobby: String, member_keys: Option<Vec<String>>) -> napi::Result<Value> {
        let client = crate::client::try_get_client()?;
        let lobby = parse_id(&lobby)?;
        let keys = member_keys
            .unwrap_or_default()
            .iter()
            .map(|key| Ok((key.clone(), c(key)?)))
            .collect::<napi::Result<Vec<_>>>()?;
        let count = unsafe { sys::SteamAPI_ISteamMatchmaking_GetNumLobbyMembers(mm(), lobby) };
        let mut members = Vec::new();
        for index in 0..count {
            let member = unsafe { sys::SteamAPI_ISteamMatchmaking_GetLobbyMemberByIndex(mm(), lobby, index) };
            let mut data = Map::new();
            for (name, key) in &keys {
                if let Some(value) = member_data(lobby, member, key) {
                    data.insert(name.clone(), json!(value));
                }
            }
            members.push(json!({
                "id": id(member),
                "name": client.friends().get_friend(SteamId::from_raw(member)).name(),
                "data": data,
            }));
        }
        let mut snapshot = lobby_summary(lobby);
        snapshot["owner"] = json!(id(unsafe { sys::SteamAPI_ISteamMatchmaking_GetLobbyOwner(mm(), lobby) }));
        snapshot["members"] = json!(members);
        Ok(snapshot)
    }

    #[napi]
    pub fn leave(lobby: String) -> napi::Result<()> {
        crate::client::try_get_client()?;
        unsafe { sys::SteamAPI_ISteamMatchmaking_LeaveLobby(mm(), parse_id(&lobby)?) };
        Ok(())
    }

    /// Native return only; owner-side and remote observation decide the outcome.
    #[napi]
    pub fn set_data(lobby: String, key: String, value: String) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        let (key, value) = (c(&key)?, c(&value)?);
        Ok(unsafe { sys::SteamAPI_ISteamMatchmaking_SetLobbyData(mm(), parse_id(&lobby)?, key.as_ptr(), value.as_ptr()) })
    }

    #[napi]
    pub fn delete_data(lobby: String, key: String) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        let key = c(&key)?;
        Ok(unsafe { sys::SteamAPI_ISteamMatchmaking_DeleteLobbyData(mm(), parse_id(&lobby)?, key.as_ptr()) })
    }

    #[napi]
    pub fn set_member_data(lobby: String, key: String, value: String) -> napi::Result<()> {
        crate::client::try_get_client()?;
        let (key, value) = (c(&key)?, c(&value)?);
        unsafe { sys::SteamAPI_ISteamMatchmaking_SetLobbyMemberData(mm(), parse_id(&lobby)?, key.as_ptr(), value.as_ptr()) };
        Ok(())
    }

    #[napi]
    pub fn get_member_data(lobby: String, member: String, key: String) -> napi::Result<Option<String>> {
        crate::client::try_get_client()?;
        Ok(member_data(parse_id(&lobby)?, parse_id(&member)?, &c(&key)?))
    }

    #[napi]
    pub fn set_owner(lobby: String, new_owner: String) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        Ok(unsafe { sys::SteamAPI_ISteamMatchmaking_SetLobbyOwner(mm(), parse_id(&lobby)?, parse_id(&new_owner)?) })
    }

    #[napi]
    pub fn set_joinable(lobby: String, joinable: bool) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        Ok(unsafe { sys::SteamAPI_ISteamMatchmaking_SetLobbyJoinable(mm(), parse_id(&lobby)?, joinable) })
    }

    /// Triggers `lobby-data-update`; works for lobbies we are not in.
    #[napi]
    pub fn request_data(lobby: String) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        Ok(unsafe { sys::SteamAPI_ISteamMatchmaking_RequestLobbyData(mm(), parse_id(&lobby)?) })
    }

    /// Up to 4 KB; every member (including the sender) receives `lobby-chat-msg`.
    #[napi]
    pub fn send_chat(lobby: String, data: Buffer) -> napi::Result<bool> {
        crate::client::try_get_client()?;
        Ok(unsafe {
            sys::SteamAPI_ISteamMatchmaking_SendLobbyChatMsg(mm(), parse_id(&lobby)?, data.as_ptr().cast(), data.len() as _)
        })
    }
}
