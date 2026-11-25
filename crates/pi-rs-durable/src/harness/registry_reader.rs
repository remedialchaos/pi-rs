use super::RegistrySnapshot;
use pi_rs_chord::{
    context::Context,
    state::{NativeObservation, NativeSource},
    watch::NativeWatchCall,
};
use std::sync::Arc;
use tokio::sync::watch;

/// Keep the most recent successful composition while the Chord registry reports a failure.
pub struct RegistryReader {
    current: watch::Sender<Arc<RegistrySnapshot>>,
    _observation: NativeObservation<Option<Arc<RegistrySnapshot>>>,
}
impl RegistryReader {
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.current.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<RegistrySnapshot>> {
        self.current.subscribe()
    }
}
pub fn registry_reader(
    source: &NativeSource<Option<Arc<RegistrySnapshot>>>,
    ctx: &Context,
) -> pi_rs_chord::Result<RegistryReader> {
    let initial = source.value().ok_or_else(|| {
        pi_rs_chord::Error::Invalid("registry has no successful composition".into())
    })?;
    let (current, _) = watch::channel(initial);
    let advanced = Arc::new(std::sync::Mutex::new(false));
    let changed = advanced.clone();
    let sender = current.clone();
    let observation = source.observe(
        ctx,
        move |call, _| {
            if let NativeWatchCall::Value {
                value: Some(snapshot),
            } = call
            {
                let mut observed = changed.lock().unwrap_or_else(|e| e.into_inner());
                *observed = true;
                sender.send_replace(snapshot);
            }
            Ok(())
        },
        |_| {},
    )?;
    let observed = advanced.lock().unwrap_or_else(|e| e.into_inner());
    if !*observed && let Some(snapshot) = observation.value.as_ref() {
        current.send_replace(snapshot.clone());
    }
    Ok(RegistryReader {
        current,
        _observation: observation,
    })
}

pub enum HarnessRegistry {
    Local(super::Registry),
    Chord(RegistryReader),
}
impl HarnessRegistry {
    pub fn snapshot(&self) -> Arc<RegistrySnapshot> {
        match self {
            Self::Local(registry) => registry.snapshot(),
            Self::Chord(reader) => reader.snapshot(),
        }
    }
    pub fn subscribe(&self) -> watch::Receiver<Arc<RegistrySnapshot>> {
        match self {
            Self::Local(registry) => registry.subscribe(),
            Self::Chord(reader) => reader.subscribe(),
        }
    }
}
impl Default for HarnessRegistry {
    fn default() -> Self {
        Self::Local(super::create_registry())
    }
}
