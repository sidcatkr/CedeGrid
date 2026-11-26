//! 003 FR-060: one task schema. The contract fixture is its export, byte for byte, and every
//! field has a name on every surface.

use cedegrid::task_schema::{FIELDS, export};
use std::collections::BTreeSet;
use std::path::Path;

const FIXTURE: &str = "tests/contracts/task-schema.json";

#[test]
fn the_fixture_is_the_schema_export() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    // CEDEGRID_WRITE_TASK_SCHEMA=1 rewrites the fixture after a deliberate schema change.
    if std::env::var_os("CEDEGRID_WRITE_TASK_SCHEMA").is_some() {
        std::fs::write(&path, export()).unwrap();
    }
    let fixture = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        fixture,
        export(),
        "{FIXTURE} is stale; rerun with CEDEGRID_WRITE_TASK_SCHEMA=1 after a deliberate change"
    );
}

#[test]
fn every_field_has_a_unique_name_on_every_surface() {
    let unique = |names: Vec<&str>| names.len() == names.iter().collect::<BTreeSet<_>>().len();
    assert!(unique(FIELDS.iter().map(|f| f.name).collect()));
    assert!(unique(FIELDS.iter().map(|f| f.typescript).collect()));
    assert!(unique(FIELDS.iter().filter_map(|f| f.flag).collect()));
    for field in FIELDS {
        assert!(
            !field.kind.is_empty() && !field.default.is_empty(),
            "{}",
            field.name
        );
        assert!(field.help.ends_with('.'), "{}: one sentence", field.name);
        assert!(!field.help.contains('\u{b7}'), "{}", field.name);
        // The TypeScript name is the Python name in camel case.
        let mut camel = String::new();
        let mut upper = false;
        for c in field.name.chars() {
            if c == '_' {
                upper = true;
            } else if upper {
                camel.extend(c.to_uppercase());
                upper = false;
            } else {
                camel.push(c);
            }
        }
        assert_eq!(field.typescript, camel, "{}", field.name);
        if let Some(flag) = field.flag {
            assert!(flag.starts_with("--") && !flag.contains('_'), "{flag}");
        }
    }
}
