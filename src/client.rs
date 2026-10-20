use std::sync::Mutex;
use steamworks::Client;

lazy_static! {
    static ref STEAM_CLIENT: Mutex<Option<Client>> = Mutex::new(None);
}

pub fn has_client() -> bool {
    STEAM_CLIENT.lock().unwrap().is_some()
}

pub fn get_client() -> Client {
    let option = STEAM_CLIENT.lock().unwrap().to_owned();
    option.unwrap()
}

/// Returns the client, or an error after shutdown / before init instead of panicking.
pub fn try_get_client() -> napi::Result<Client> {
    STEAM_CLIENT
        .lock()
        .unwrap()
        .to_owned()
        .ok_or_else(|| napi::Error::from_reason("Steam is not initialized"))
}

pub fn set_client(client: Client) {
    let mut client_ref = STEAM_CLIENT.lock().unwrap();
    *client_ref = Some(client);
}

pub fn drop_client() {
    let mut client_ref = STEAM_CLIENT.lock().unwrap();
    *client_ref = None;
}
