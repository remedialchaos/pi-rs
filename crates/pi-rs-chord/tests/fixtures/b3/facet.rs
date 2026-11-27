// Regenerate this component with the instructions in README.md.
wit_bindgen::generate!({ world: "plugin" });

use chord::plugin::host::{publish, read};
use serde_json::{Value, json};

struct Facet;

impl Guest for Facet {
    fn setup() -> Result<String, String> {
        Ok(include_str!("facet.json").to_owned())
    }

    fn invoke(
        object: String,
        member: String,
        args: String,
        principal: String,
    ) -> Result<String, String> {
        let value: Value = serde_json::from_str(&args).map_err(|error| error.to_string())?;
        match member.as_str() {
            "add" => {
                let document: Value = serde_json::from_str(&read(&object, "total", None)?)
                    .map_err(|error| error.to_string())?;
                let next = document["total"].as_i64().ok_or("missing document total")?
                    + value.as_i64().ok_or("expected integer argument")?;
                publish(&object, "total", &json!({"total": next}).to_string())?;
                publish(&object, "tick", &next.to_string())?;
                publish(
                    &object,
                    "rows",
                    &json!({"key":"a","value":next}).to_string(),
                )?;
                read(&object, "rows", Some("\"a\""))
            }
            "update" => {
                let old = value["value"]["total"]
                    .as_i64()
                    .ok_or("missing document value")?;
                let delta = value["args"].as_i64().ok_or("missing delta")?;
                if delta < 0 {
                    return Err("negative change rejected".into());
                }
                let next = old + delta;
                Ok(json!({"value":{"total":next},"result":old}).to_string())
            }
            "foreign" => {
                let other = object.replace(r#""room":"alpha""#, r#""room":"other""#);
                read(&other, "total", None)
            }
            "echo" => {
                let principal: Value =
                    serde_json::from_str(&principal).map_err(|error| error.to_string())?;
                Ok(json!({
                    "params": value["params"],
                    "body": value["body"],
                    "principal": principal
                })
                .to_string())
            }
            "who" => Ok(principal),
            _ => Err("unknown guest callback".into()),
        }
    }
}

export!(Facet);

fn main() {}
