use super::*;
#[derive(Default)]
struct Graph {
    edges: BTreeMap<String, Vec<String>>,
    waits: Vec<Weak<Waiting>>,
}
struct Waiting {
    target: String,
    chain: BTreeSet<String>,
    error: watch::Sender<Option<ChordError>>,
}
#[derive(Clone, Default)]
pub struct ReloadCycles(Arc<Mutex<Graph>>);
pub struct CycleWait(Arc<Waiting>);
impl ReloadCycles {
    pub fn set_edges(&self, object: impl Into<String>, edges: Vec<String>) {
        let mut graph = lock(&self.0);
        graph.edges.insert(object.into(), edges);
        Self::recheck(&mut graph);
    }
    pub fn remove_edges(&self, object: &str) {
        let mut graph = lock(&self.0);
        graph.edges.remove(object);
        Self::recheck(&mut graph);
    }
    pub fn watch(&self, target: impl Into<String>, chain: Vec<String>) -> CycleWait {
        let (error, _) = watch::channel(None);
        let waiting = Arc::new(Waiting {
            target: target.into(),
            chain: chain.into_iter().collect(),
            error,
        });
        let mut graph = lock(&self.0);
        graph.waits.push(Arc::downgrade(&waiting));
        Self::recheck(&mut graph);
        CycleWait(waiting)
    }
    fn recheck(graph: &mut Graph) {
        graph.waits.retain(|w| w.strong_count() > 0);
        for wait in graph.waits.iter().filter_map(Weak::upgrade) {
            if wait.error.borrow().is_some() {
                continue;
            }
            let mut visited = BTreeSet::from([wait.target.clone()]);
            let mut stack = vec![(wait.target.clone(), vec![wait.target.clone()])];
            while let Some((at, path)) = stack.pop() {
                for next in graph.edges.get(&at).into_iter().flatten() {
                    let mut path = path.clone();
                    path.push(next.clone());
                    if wait.chain.contains(next) {
                        let mut error = ChordError::new(
                            "unavailable",
                            format!(
                                "reload drains work waiting for its own drain: {}",
                                path.join(" -> ")
                            ),
                        );
                        error.reason = Some("reload_cycle".into());
                        wait.error.send_replace(Some(error));
                        break;
                    }
                    if visited.insert(next.clone()) {
                        stack.push((next.clone(), path));
                    }
                }
                if wait.error.borrow().is_some() {
                    break;
                }
            }
        }
    }
}
impl CycleWait {
    pub fn error(&self) -> Option<ChordError> {
        self.0.error.borrow().clone()
    }
    pub async fn cut(&self) -> ChordError {
        let mut receiver = self.0.error.subscribe();
        loop {
            if let Some(error) = receiver.borrow_and_update().clone() {
                return error;
            }
            if receiver.changed().await.is_err() {
                return ChordError::new("unavailable", "reload cycle watch closed");
            }
        }
    }
}
