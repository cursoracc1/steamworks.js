use napi_derive::napi;

#[napi]
pub mod runtime {
    use serde_json::{json, Value};

    /// Returns and clears queued events in arrival order.
    #[napi(ts_return_type = "Array<import('./steam-events').SteamEvent>")]
    pub fn drain_events() -> Vec<Value> {
        crate::events::drain()
    }

    /// Callback pump timing since the last reset, plus queue depth and overflow.
    #[napi(ts_return_type = "import('./steam-events').PumpStats")]
    pub fn pump_stats(reset: Option<bool>) -> Value {
        crate::events::pump_stats(reset.unwrap_or(false))
    }

    #[napi(ts_return_type = "import('./steam-events').Identity")]
    pub fn identity() -> napi::Result<Value> {
        let client = crate::client::try_get_client()?;
        Ok(json!({
            "steamId": client.user().steam_id().raw().to_string(),
            "name": client.friends().name(),
            "appId": client.utils().app_id().0,
            "loggedOn": client.user().logged_on(),
            "interfaces": {
                "matchmaking": "SteamMatchMaking009",
                "networkingSockets": "SteamNetworkingSockets013",
                "networkingUtils": "SteamNetworkingUtils004",
            },
        }))
    }
}
