//! The real document, `docs/openapi.json`: it resolves, and every language
//! emits every operation and every model of it.

mod common;

#[test]
fn every_operation_and_model_is_emitted() {
    let spec = common::spec("../../docs/openapi.json");
    assert!(!spec.operations.is_empty() && !spec.models.is_empty());
    for (language, emit) in common::languages() {
        let files = emit(&spec);
        let text: String = files.values().map(String::as_str).collect();
        for op in &spec.operations {
            assert!(
                text.contains(&format!("\"{}\"", op.id)) || text.contains(&format!("'{}'", op.id)),
                "{language}: operation {} missing",
                op.id
            );
        }
        for model in &spec.models {
            let name = model.name();
            let forms = [
                format!("interface {name} "),
                format!("type {name} "),
                format!("struct {name} "),
                format!("enum {name} "),
                format!("class {name}:"),
                format!("class {name}("),
                format!("{name}: TypeAlias"),
                format!("class {name}\n"),
                format!("enum {name}: string"),
                format!("function {}FromJson(", cligen::name::camel(name)),
            ];
            assert!(
                forms.iter().any(|form| text.contains(form.as_str())),
                "{language}: model {name} missing"
            );
        }
        println!(
            "{language}: {} files, {} operations, {} models",
            files.len(),
            spec.operations.len(),
            spec.models.len()
        );
    }
}
