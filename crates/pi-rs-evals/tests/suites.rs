use pi_rs_evals::suites::{SuiteGroup, discover};
use std::path::Path;

#[test]
fn documentation_discovery_keeps_both_template_scopes_and_excludes_helpers()
-> Result<(), Box<dyn std::error::Error>> {
    let cases = discover(Path::new("."), SuiteGroup::Documentation)?;
    assert_eq!(cases.len(), 7);
    let template: Vec<_> = cases
        .iter()
        .filter(|c| c.file == "src/suites/prompt_templates.rs")
        .map(|c| c.eval_set.as_str())
        .collect();
    assert_eq!(
        template,
        [
            "Create a project prompt template",
            "Create a user prompt template"
        ]
    );
    for c in cases {
        assert!(Path::new(env!("CARGO_MANIFEST_DIR")).join(c.file).is_file());
        assert_eq!(c.full_name, format!("{} > {}", c.eval_set, c.case_id));
    }
    Ok(())
}

#[test]
fn audit_discovers_each_nested_documentation_page_in_lexical_order()
-> Result<(), Box<dyn std::error::Error>> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("crates/pi-rs-coding-agent/assets/docs");
    std::fs::create_dir_all(root.join("nested"))?;
    std::fs::write(root.join("z.md"), "another page")?;
    std::fs::write(root.join("nested/a.md"), "audited page")?;
    std::fs::write(root.join("ignored.txt"), "not a documentation page")?;
    let cases = discover(tmp.path(), SuiteGroup::Audit)?;
    assert_eq!(
        cases.iter().map(|c| c.case_id.as_str()).collect::<Vec<_>>(),
        [
            "nested/a.md matches the implementation",
            "z.md matches the implementation"
        ]
    );
    Ok(())
}
