//! Browser storage helpers (`localStorage`, `sessionStorage`, and IndexedDB `p2-conversations`).

use crate::state::conversation_cache::{
    CachedConversation, INDEXED_DB_NAME, INDEXED_DB_STORE, INDEXED_DB_VERSION,
    PersistedTranscriptStore,
};
use futures_channel::oneshot;
use leptos::prelude::window;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

pub fn local_get(key: &str) -> Option<String> {
    let storage = window().local_storage().ok().flatten()?;
    storage.get_item(key).ok().flatten()
}

pub fn local_set(key: &str, value: &str) -> bool {
    let Ok(Some(storage)) = window().local_storage() else {
        return false;
    };
    storage.set_item(key, value).is_ok()
}

pub fn local_remove(key: &str) {
    if let Ok(Some(storage)) = window().local_storage() {
        let _ = storage.remove_item(key);
    }
}

pub fn session_get(key: &str) -> Option<String> {
    let storage = window().session_storage().ok().flatten()?;
    storage.get_item(key).ok().flatten()
}

pub fn session_set(key: &str, value: &str) -> bool {
    let Ok(Some(storage)) = window().session_storage() else {
        return false;
    };
    storage.set_item(key, value).is_ok()
}

pub fn session_remove(key: &str) {
    if let Ok(Some(storage)) = window().session_storage() {
        let _ = storage.remove_item(key);
    }
}

async fn open_conversations_db() -> Option<JsValue> {
    let idb = js_sys::Reflect::get(&window(), &"indexedDB".into()).ok()?;
    if idb.is_undefined() || idb.is_null() {
        return None;
    }
    let open_fn = js_sys::Reflect::get(&idb, &"open".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let req = open_fn
        .call2(
            &idb,
            &JsValue::from_str(INDEXED_DB_NAME),
            &JsValue::from_f64(f64::from(INDEXED_DB_VERSION)),
        )
        .ok()?;

    let req_upgrade = req.clone();
    let on_upgrade = Closure::<dyn FnMut(JsValue)>::new(move |_ev: JsValue| {
        if let Ok(db) = js_sys::Reflect::get(&req_upgrade, &"result".into())
            && let Ok(names) = js_sys::Reflect::get(&db, &"objectStoreNames".into())
            && let Ok(contains_fn) = js_sys::Reflect::get(&names, &"contains".into())
                .and_then(|v| v.dyn_into::<js_sys::Function>())
        {
            let exists = contains_fn
                .call1(&names, &JsValue::from_str(INDEXED_DB_STORE))
                .ok()
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !exists
                && let Ok(create_fn) = js_sys::Reflect::get(&db, &"createObjectStore".into())
                    .and_then(|v| v.dyn_into::<js_sys::Function>())
            {
                let opts = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&opts, &"keyPath".into(), &"key".into());
                let _ = create_fn.call2(&db, &JsValue::from_str(INDEXED_DB_STORE), &opts);
            }
        }
    });
    let _ = js_sys::Reflect::set(&req, &"onupgradeneeded".into(), on_upgrade.as_ref());
    on_upgrade.forget();

    let (tx, rx) = oneshot::channel::<Option<JsValue>>();
    let tx_ok = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let tx_err = tx_ok.clone();
    let req_ok = req.clone();
    let on_success = Closure::<dyn FnMut(JsValue)>::new(move |_ev: JsValue| {
        let db = js_sys::Reflect::get(&req_ok, &"result".into()).ok();
        if let Some(sender) = tx_ok.borrow_mut().take() {
            let _ = sender.send(db);
        }
    });
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_ev: JsValue| {
        if let Some(sender) = tx_err.borrow_mut().take() {
            let _ = sender.send(None);
        }
    });
    let _ = js_sys::Reflect::set(&req, &"onsuccess".into(), on_success.as_ref());
    let _ = js_sys::Reflect::set(&req, &"onerror".into(), on_error.as_ref());
    on_success.forget();
    on_error.forget();

    rx.await.ok().flatten()
}

fn store_from_db(db: &JsValue, mode: &str) -> Option<JsValue> {
    let tx_fn = js_sys::Reflect::get(db, &"transaction".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let tx = tx_fn
        .call2(
            db,
            &JsValue::from_str(INDEXED_DB_STORE),
            &JsValue::from_str(mode),
        )
        .ok()?;
    let os_fn = js_sys::Reflect::get(&tx, &"objectStore".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    os_fn.call1(&tx, &JsValue::from_str(INDEXED_DB_STORE)).ok()
}

/// Loads all cached conversations from IndexedDB `p2-conversations`.
pub async fn idb_load_all() -> Option<PersistedTranscriptStore> {
    let db = open_conversations_db().await?;
    let store = store_from_db(&db, "readonly")?;
    let get_all = js_sys::Reflect::get(&store, &"getAll".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let req = get_all.call0(&store).ok()?;
    let (tx, rx) = oneshot::channel::<Option<JsValue>>();
    let tx_ok = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
    let tx_err = tx_ok.clone();
    let req_ok = req.clone();
    let on_success = Closure::<dyn FnMut(JsValue)>::new(move |_ev: JsValue| {
        let res = js_sys::Reflect::get(&req_ok, &"result".into()).ok();
        if let Some(sender) = tx_ok.borrow_mut().take() {
            let _ = sender.send(res);
        }
    });
    let on_error = Closure::<dyn FnMut(JsValue)>::new(move |_ev: JsValue| {
        if let Some(sender) = tx_err.borrow_mut().take() {
            let _ = sender.send(None);
        }
    });
    let _ = js_sys::Reflect::set(&req, &"onsuccess".into(), on_success.as_ref());
    let _ = js_sys::Reflect::set(&req, &"onerror".into(), on_error.as_ref());
    on_success.forget();
    on_error.forget();

    let result = rx.await.ok().flatten()?;
    let json_str = js_sys::JSON::stringify(&result).ok()?.as_string()?;
    let records: Vec<CachedConversation> = serde_json::from_str(&json_str).ok()?;
    let mut out = PersistedTranscriptStore::new();
    for conv in records {
        out.conversations.insert(conv.key.clone(), conv);
    }
    Some(out)
}

/// Persists a single `CachedConversation` into IndexedDB `p2-conversations`.
pub async fn idb_put_conversation(conv: CachedConversation) -> bool {
    let Some(db) = open_conversations_db().await else {
        return false;
    };
    let Some(store) = store_from_db(&db, "readwrite") else {
        return false;
    };
    let Ok(json_str) = serde_json::to_string(&conv) else {
        return false;
    };
    let Ok(js_val) = js_sys::JSON::parse(&json_str) else {
        return false;
    };
    let Ok(put_fn) =
        js_sys::Reflect::get(&store, &"put".into()).and_then(|v| v.dyn_into::<js_sys::Function>())
    else {
        return false;
    };
    put_fn.call1(&store, &js_val).is_ok()
}

/// Deletes all conversations for `session_id` from IndexedDB `p2-conversations`.
pub async fn idb_delete_session(session_id: String) -> bool {
    let Some(existing) = idb_load_all().await else {
        return false;
    };
    let Some(db) = open_conversations_db().await else {
        return false;
    };
    let Some(store) = store_from_db(&db, "readwrite") else {
        return false;
    };
    let Ok(del_fn) = js_sys::Reflect::get(&store, &"delete".into())
        .and_then(|v| v.dyn_into::<js_sys::Function>())
    else {
        return false;
    };
    for (key, conv) in existing.conversations {
        if conv.session_id == session_id {
            let _ = del_fn.call1(&store, &JsValue::from_str(&key));
        }
    }
    true
}
