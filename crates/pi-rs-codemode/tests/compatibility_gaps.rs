//! Original Boa incompatibility, retained as the ADR-010 regression gate.
use pi_rs_codemode::*;
use serde_json::json;
#[test]
fn deep_recursion_is_a_catchable_range_error_after_1000_frames() -> Result<(), HostError> {
    let result=futures::executor::block_on(CodemodeSandbox::default().execute(
        "let depth=0;function dive(){depth++;dive()}try{dive()}catch(error){return [error.name,depth>1000]}",
        ExecuteOptions::default(),
    ))?;
    assert_eq!(
        result.value(),
        Some(&json!(["RangeError", true])),
        "{result:?}"
    );
    Ok(())
}
