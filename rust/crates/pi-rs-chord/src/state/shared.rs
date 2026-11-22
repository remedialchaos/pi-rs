use crate::{
    Result,
    context::lock,
    delta::{Revision, WeakRevision},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
type Mapper = Arc<dyn Fn(&Revision) -> Result<Revision> + Send + Sync>;
/// Identity reuse is for container elements only. Primitive elements are mapped on every call.
pub struct MapShared {
    mapper: Mapper,
    cache: Mutex<HashMap<usize, (WeakRevision, Revision)>>,
}
pub fn map_shared(
    mapper: impl Fn(&Revision) -> Result<Revision> + Send + Sync + 'static,
) -> MapShared {
    MapShared {
        mapper: Arc::new(mapper),
        cache: Mutex::new(HashMap::new()),
    }
}
impl MapShared {
    pub fn map(&self, elements: &[Revision]) -> Result<Vec<Revision>> {
        lock(&self.cache).retain(|_, (input, _)| input.upgrade().is_some());
        elements
            .iter()
            .map(|input| {
                if !input.is_container() {
                    return (self.mapper)(input);
                }
                let id = input.identity();
                let found = lock(&self.cache)
                    .get(&id)
                    .filter(|(key, _)| key.upgrade().is_some_and(|key| key.ptr_eq(input)))
                    .map(|(_, value)| value.clone());
                if let Some(value) = found {
                    return Ok(value);
                }
                let result = (self.mapper)(input)?;
                lock(&self.cache).insert(id, (input.downgrade(), result.clone()));
                Ok(result)
            })
            .collect()
    }
}
