use pi_rs_chord::{
    context::Context,
    registries::*,
    state::{Batch, NativeSource},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn record() -> pi_rs_chord::Result<RegistryRecord<i64, i64>> {
    let token = define_registry("test/sum", 2)?;
    Ok(RegistryRecord::new(token, Batch::new(|_| {}), |_| {}))
}
fn provider() -> RegistryProvider<i64, i64> {
    RegistryProvider::new(|table, _| Ok(table.iter().map(|e| e.value).sum()))
}
#[test]
fn registry_checks_names_and_positive_versions() {
    assert!(define_registry::<i64, i64>("Bad Name", 1).is_err());
    assert!(define_registry::<i64, i64>("good", 0).is_err());
}
#[test]
fn registry_orders_owners_and_removes_only_the_selected_owner() -> pi_rs_chord::Result<()> {
    let record = record()?;
    let ctx = Context::background();
    record.serve(provider(), &ctx)?;
    record.contribute(Contributor::facet("z"), 2, 3, &ctx)?;
    record.contribute(Contributor::object("a", "z"), 2, 5, &ctx)?;
    record.contribute(Contributor::facet("a"), 2, 7, &ctx)?;
    assert_eq!(
        record
            .table()?
            .iter()
            .map(|e| (&*e.plugin, e.object_type.as_deref()))
            .collect::<Vec<_>>(),
        vec![("a", None), ("a", Some("z")), ("z", None)]
    );
    assert_eq!(record.value(2)?, 15);
    record.remove_owner(&Contributor::facet("a"), &ctx)?;
    assert_eq!(record.value(2)?, 8);
    Ok(())
}
#[test]
fn registry_rejects_unaccepted_versions_and_adapts_both_directions() -> pi_rs_chord::Result<()> {
    let record = record()?;
    let ctx = Context::background();
    record.contribute(Contributor::facet("old"), 1, 4, &ctx)?;
    record.contribute(Contributor::facet("new"), 3, 100, &ctx)?;
    record.serve(
        provider().accept(1, |old| Ok(old * 10), |value| Ok(value / 10))?,
        &ctx,
    )?;
    assert_eq!(record.value(2)?, 40);
    assert_eq!(record.value(1)?, 4);
    assert!(record.value(3).is_err());
    Ok(())
}
#[test]
fn registry_follows_contributions_and_atomic_replacement_has_one_publication()
-> pi_rs_chord::Result<()> {
    let batch = Batch::new(|_| {});
    let ctx = Context::background();
    let record = RegistryRecord::new(define_registry("test/live", 1)?, batch.clone(), |_| {});
    let source = NativeSource::new(batch, 2_i64, |a, b| a == b);
    record.serve(provider(), &ctx)?;
    record.follow(Contributor::facet("a"), 1, source.clone(), &ctx)?;
    let publications = Arc::new(AtomicUsize::new(0));
    let count = publications.clone();
    let values = record.source();
    let _observer = values.observe(
        &ctx,
        move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        |_| {},
    )?;
    source.set(3, &ctx)?;
    assert_eq!(record.value(1)?, 3);
    assert_eq!(publications.load(Ordering::SeqCst), 1);
    record.replace(
        provider(),
        vec![
            (Contributor::facet("b"), 1, 7),
            (Contributor::facet("c"), 1, 11),
        ],
        &ctx,
    )?;
    assert_eq!(record.value(1)?, 18);
    assert_eq!(publications.load(Ordering::SeqCst), 2);
    source.set(100, &ctx)?;
    assert_eq!(record.value(1)?, 18);
    Ok(())
}
#[test]
fn registry_compose_failure_keeps_last_good_until_an_owner_leaves() -> pi_rs_chord::Result<()> {
    let record = record()?;
    let ctx = Context::background();
    record.serve(provider(), &ctx)?;
    record.contribute(Contributor::facet("a"), 2, 5, &ctx)?;
    record.serve(
        RegistryProvider::new(|_, _| Err(pi_rs_chord::Error::Invalid("broken compose".into()))),
        &ctx,
    )?;
    assert_eq!(record.last_good(), Some(5));
    assert!(record.value(2).is_err());
    record.remove_owner(&Contributor::facet("a"), &ctx)?;
    assert_eq!(record.last_good(), None);
    Ok(())
}
#[test]
fn keyed_entries_keep_first_owner_and_report_conflicts() {
    let table = vec![
        RegistryEntry {
            plugin: "a".into(),
            object_type: None,
            version: 1,
            value: vec![("x".to_string(), 1)],
        },
        RegistryEntry {
            plugin: "b".into(),
            object_type: None,
            version: 1,
            value: vec![("x".to_string(), 2)],
        },
    ];
    let mut context = ComposeContext::default();
    let entries = keyed_entries(&table, |e| e.clone(), &mut context);
    assert_eq!(entries["x"].value, 1);
    assert_eq!(context.conflicts[0].plugins, vec!["a", "b"]);
}
