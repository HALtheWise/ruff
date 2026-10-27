//! Tests of loading plugins. The behavior of their checks on Python code is tested by the
//! snapshot tests in `ruff_linter`, which provides the semantic model.

use std::path::PathBuf;

use crate::{OptionValue, PluginSpec, load};

fn spec(options: Vec<(String, OptionValue)>) -> PluginSpec {
    PluginSpec {
        name: "test".to_string(),
        path: PathBuf::from("test.star"),
        root: PathBuf::from("."),
        options,
    }
}

/// Loads a plugin and returns the error, as the user would see it.
fn load_error(source: &str) -> String {
    load_error_with_options(source, Vec::new())
}

fn load_error_with_options(source: &str, options: Vec<(String, OptionValue)>) -> String {
    let Err(error) = load::load(&spec(options), source) else {
        panic!("Expected the plugin to fail to load");
    };
    format!("{error:#}")
}

const RULE: &str = "R = rule(\"T001\", \"t\")\n";

#[test]
fn unknown_kind_suggests_a_kind() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}nodes(\"Cal\").foreach(lambda node: None)"
    )));
}

#[test]
fn unqualified_builtin() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}calls(\"print\").foreach(lambda node: None)"
    )));
}

#[test]
fn invalid_qualified_name() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}calls(\"requests/get\").foreach(lambda node: None)"
    )));
}

#[test]
fn invalid_regex() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}comments(\"(\").foreach(lambda node: None)"
    )));
}

#[test]
fn invalid_path_pattern() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}calls().filter_path(\"src/[\").foreach(lambda node: None)"
    )));
}

#[test]
fn filter_name_of_nodes_without_names() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}calls().filter_name(\"^get$\").foreach(lambda node: None)"
    )));
}

#[test]
fn foreach_without_a_function() {
    insta::assert_snapshot!(load_error(&format!("{RULE}calls().foreach(\"check\")")));
}

#[test]
fn report_while_loading() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}R.report(None, \"m\")\ncalls().foreach(lambda node: None)"
    )));
}

#[test]
fn edit_while_loading() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}FIX = replace(None, \"x\")\ncalls().foreach(lambda node: None)"
    )));
}

#[test]
fn invalid_rule_code() {
    insta::assert_snapshot!(load_error(
        "rule(\"acme1\", \"t\")\ncalls().foreach(lambda node: None)"
    ));
}

#[test]
fn invalid_rule_name() {
    insta::assert_snapshot!(load_error(
        "rule(\"ACME001\", \"No Print\")\ncalls().foreach(lambda node: None)"
    ));
}

#[test]
fn duplicate_rule_code() {
    insta::assert_snapshot!(load_error(&format!(
        "{RULE}{RULE}calls().foreach(lambda node: None)"
    )));
}

#[test]
fn unknown_option() {
    insta::assert_snapshot!(load_error_with_options(
        &format!(
            "LAYERS = option(\"layers\", default = [], help = \"Module prefixes, from highest to lowest.\")\nSTRICT = option(\"strict\", default = False)\n{RULE}calls().foreach(lambda node: None)"
        ),
        vec![("layres".to_string(), OptionValue::Array(Vec::new()))],
    ));
}

#[test]
fn option_of_the_wrong_type() {
    insta::assert_snapshot!(load_error_with_options(
        &format!(
            "LAYERS = option(\"layers\", default = [])\n{RULE}calls().foreach(lambda node: None)"
        ),
        vec![("layers".to_string(), OptionValue::String("app".to_string()))],
    ));
}

#[test]
fn no_rules() {
    insta::assert_snapshot!(load_error("calls().foreach(lambda node: None)"));
}

#[test]
fn no_checks() {
    insta::assert_snapshot!(load_error(RULE));
}

/// Every field that holds nodes has a method, and every method is the field of some kind that
/// holds nodes, so that the tables in `kind.rs` and `values.rs` can't drift apart unnoticed.
#[test]
fn fields_that_hold_nodes_have_methods() {
    use crate::kind::Kind;

    let methods: Vec<&str> = crate::values::field_methods::NAMES
        .iter()
        .map(|name| name.trim_start_matches("r#"))
        .collect();
    for kind in Kind::ALL {
        for field in kind.fields() {
            assert!(
                kind.is_data_field(field) || methods.contains(field),
                "`{}.{field}` holds nodes, but has no method",
                kind.name()
            );
        }
    }
    for method in methods {
        assert!(
            Kind::ALL
                .iter()
                .any(|kind| kind.fields().contains(&method) && !kind.is_data_field(method)),
            "`{method}` has a method, but no kind has a field `{method}` that holds nodes"
        );
    }
}
