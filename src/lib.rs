use napi::bindgen_prelude::Error;
use napi_derive::napi;
use steamworks::AppId;
use steamworks::Client;
use steamworks::SteamAPIInitError;

pub mod client;
pub mod events;

#[macro_use]
extern crate lazy_static;

#[napi]
pub fn init(app_id: Option<u32>) -> Result<(), Error> {
    if client::has_client() {
        shutdown();
    }

    let steam_client = app_id
        .map(|app_id| Client::init_app(AppId(app_id)))
        .unwrap_or_else(Client::init)
        .map_err(|e| match e {
            SteamAPIInitError::Generic(msg)
            | SteamAPIInitError::NoSteamClient(msg)
            | SteamAPIInitError::VersionMismatch(msg) => Error::from_reason(msg),
        })?;

    events::register_raw(&steam_client);
    client::set_client(steam_client);
    Ok(())
}

#[napi]
pub fn restart_app_if_necessary(app_id: u32) -> bool {
    steamworks::restart_app_if_necessary(AppId(app_id))
}

/// The single callback pump. A no-op once shut down, so a late timer tick is harmless.
#[napi]
pub fn run_callbacks() {
    let Ok(client) = client::try_get_client() else {
        return;
    };
    let started = std::time::Instant::now();
    client.process_callbacks(events::on_callback);
    events::record_pump(started);
}

/// Closes tracked sockets, unregisters raw callbacks and releases the client
/// (SteamAPI_Shutdown runs when the last client reference drops).
#[napi]
pub fn shutdown() {
    if !client::has_client() {
        return;
    }
    api::sockets::shutdown_all();
    events::clear();
    client::drop_client();
}

pub mod api;
