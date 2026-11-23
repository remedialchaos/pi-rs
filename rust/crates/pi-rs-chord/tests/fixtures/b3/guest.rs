// Compiled against a copy of the crate's wit/plugin.wit. See README.md.
wit_bindgen::generate!({ world: "plugin" });

use std::cell::Cell;

thread_local! {
    static TOTAL: Cell<i32> = const { Cell::new(0) };
}

struct Counter;

impl Guest for Counter {
    fn setup() -> Result<String, String> {
        if option_env!("B3_REVISION") == Some("reject") {
            chord::plugin::host::log("rejecting replacement");
            return Err("fixture rejected setup".into());
        }
        Ok(r#"[{"name":"guest-counter","version":1,"members":{"total":{"kind":"state","schema":{"type":"integer"},"initial":0},"add":{"kind":"method","args":{"type":"integer"},"result":{"type":"integer"}}}}]"#.into())
    }

    fn invoke(
        object: String,
        member: String,
        args_json: String,
        _: String,
    ) -> Result<String, String> {
        if member != "add" {
            return Err("unexpected method".into());
        }
        let delta: i32 = args_json.parse().map_err(|_| "expected an integer")?;
        let multiplier = if option_env!("B3_REVISION") == Some("double") {
            2
        } else {
            1
        };
        TOTAL.with(|total| {
            let next = total.get() + multiplier * delta;
            let json = next.to_string();
            chord::plugin::host::publish(&object, "total", &json)?;
            total.set(next);
            Ok(json)
        })
    }
}

export!(Counter);

fn main() {}
