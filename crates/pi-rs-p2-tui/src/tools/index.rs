use crate::preferences::TuiSettings;
use crate::tools::types::{ToolRendererResolver, ToolRenderers};
use std::collections::HashMap;

pub fn resolve_tool_renderers(
    name: &str,
    resolvers: &HashMap<String, ToolRendererResolver>,
    registered: &HashMap<String, ToolRenderers>,
    settings: &TuiSettings,
) -> Option<ToolRenderers> {
    let mut ordered: Vec<_> = resolvers.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(b.0));

    let own = registered.get(name).cloned();

    fn at(
        index: usize,
        ordered: &[(&String, &ToolRendererResolver)],
        own: Option<ToolRenderers>,
        name: &str,
        settings: &TuiSettings,
    ) -> Option<ToolRenderers> {
        if index >= ordered.len() {
            return own;
        }
        let (_, resolver) = &ordered[index];
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resolver(
                name,
                &|| at(index + 1, ordered, own.clone(), name, settings),
                settings,
            )
        })) {
            Ok(result) => result,
            Err(_) => at(index + 1, ordered, own, name, settings),
        }
    }

    let drawn = at(0, &ordered, own.clone(), name, settings);
    let nested_calls = own.as_ref().and_then(|o| o.nested_calls.clone());
    if nested_calls.is_none() {
        return drawn;
    }
    let mut drawn = drawn?;
    drawn.nested_calls = nested_calls;
    Some(drawn)
}
