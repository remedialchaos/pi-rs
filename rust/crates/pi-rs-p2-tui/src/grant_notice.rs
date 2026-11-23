#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiGrantResult {
    Ok,
    Denied,
    Failed,
    Other(String),
}
pub fn grant_notice(mut notify: impl FnMut(&str, bool), done: &str, result: TuiGrantResult) {
    match result {
        TuiGrantResult::Ok => notify(done, false),
        TuiGrantResult::Denied => {}
        TuiGrantResult::Failed => notify("The launcher could not do that (see its message)", true),
        TuiGrantResult::Other(error) => notify(&format!("Not done ({error})"), true),
    }
}
