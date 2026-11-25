use pi_rs_chord::data::join_key;
use serde_json::{Value, json};

pub fn server_address() -> Value {
    json!({"kind":"server","params":{},"key":"server"})
}

pub fn session_address(id: &str) -> Value {
    let key = join_key(&["session".into(), id.into()]);
    json!({"kind":"session","params":{"id":id},"key":key})
}
