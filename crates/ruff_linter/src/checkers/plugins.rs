//! Running Starlark plugin rules (see [`ruff_plugin`]) as part of the AST traversal.

use ruff_db::diagnostic::{
    Annotation, Diagnostic, DiagnosticId, LintName, SecondaryCode, Severity, Span,
};
use ruff_diagnostics::Edit;
use ruff_plugin::{Ancestor, Finding, Host, PluginError};
use ruff_python_ast::{Arguments, Expr, Stmt};
use ruff_source_file::SourceFile;
use ruff_text_size::TextRange;

use crate::checkers::ast::{Checker, LintContext};
use crate::fix::edits::{Parentheses, delete_stmt, remove_argument};

/// Gives plugins access to the state of the [`Checker`] at the node being visited.
pub(crate) struct PluginHost<'a, 'b> {
    checker: &'b Checker<'a>,
}

impl<'a, 'b> PluginHost<'a, 'b> {
    pub(crate) fn new(checker: &'b Checker<'a>) -> Self {
        Self { checker }
    }
}

impl<'a> Host<'a> for PluginHost<'a, '_> {
    fn source(&self) -> &'a str {
        self.checker.locator().contents()
    }

    fn source_file(&self) -> SourceFile {
        self.checker.context().source_file().clone()
    }

    fn qualified_name(&self, expr: &'a Expr) -> Option<String> {
        let name = self.checker.semantic().resolve_qualified_name(expr)?;
        Some(match name.segments() {
            ["", builtin @ ..] if !builtin.is_empty() => format!("builtins.{}", builtin.join(".")),
            _ => name.to_string(),
        })
    }

    fn ancestors(&self) -> Vec<Ancestor<'a>> {
        let semantic = self.checker.semantic();
        semantic
            .current_expressions()
            .map(Ancestor::Expr)
            .chain(semantic.current_statements().map(Ancestor::Stmt))
            .collect()
    }

    fn line_ending(&self) -> &'static str {
        self.checker.stylist().line_ending().as_str()
    }

    fn delete_statement(&self, stmt: &'a Stmt, parent: Option<&'a Stmt>) -> Edit {
        delete_stmt(stmt, parent, self.checker.locator(), self.checker.indexer())
    }

    fn delete_argument(
        &self,
        argument: TextRange,
        arguments: &'a Arguments,
    ) -> anyhow::Result<Edit> {
        remove_argument(
            &argument,
            arguments,
            Parentheses::Preserve,
            self.checker.locator().contents(),
            self.checker.tokens(),
        )
    }
}

impl LintContext<'_> {
    /// Reports what the plugin rules found in the current file.
    pub(crate) fn report_plugin_results(&self, findings: Vec<Finding>, errors: Vec<PluginError>) {
        for finding in findings {
            let mut diagnostic = Diagnostic::new(
                DiagnosticId::Lint(LintName::of(finding.rule.name)),
                Severity::Error,
                finding.message,
            );
            let span = Span::from(self.source_file().clone()).with_range(finding.range);
            diagnostic.annotate(Annotation::primary(span));
            if let Some(fix_title) = finding.fix_title {
                diagnostic.help(fix_title);
            }
            if let Some(fix) = finding.fix {
                diagnostic.set_fix(fix);
            }
            diagnostic.set_secondary_code(SecondaryCode::new(finding.rule.code.clone()));
            diagnostic.set_documentation_url(finding.rule.url.clone());
            self.push_diagnostic(diagnostic);
        }
        // A failing callback is a diagnostic, rather than a warning, so that it fails the run,
        // isn't hidden by the cache, and shows up in editors. Like a syntax error, it can't be
        // suppressed: only fixing the plugin removes it.
        for error in errors {
            let location = match &error.location {
                Some((path, line)) => {
                    format!(" (at {}:{line})", crate::fs::relativize_path(path))
                }
                None => String::new(),
            };
            let message = format!(
                "Plugin `{}` failed: {}{location}",
                error.plugin, error.message
            );
            let mut diagnostic =
                Diagnostic::new(DiagnosticId::PluginError, Severity::Error, message);
            let span = Span::from(self.source_file().clone()).with_range(error.range);
            diagnostic.annotate(Annotation::primary(span));
            diagnostic.info(error.details);
            if let Some(hint) = error.hint {
                diagnostic.help(hint);
            }
            self.push_diagnostic(diagnostic);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use ruff_plugin::{OptionValue, PluginSet, PluginSpec};
    use test_case::test_case;

    use crate::settings::LinterSettings;
    use crate::test::{test_path, test_resource_path};
    use crate::{assert_diagnostics, settings};

    fn strings(values: &[&str]) -> OptionValue {
        OptionValue::Array(
            values
                .iter()
                .map(|value| OptionValue::String((*value).to_string()))
                .collect(),
        )
    }

    /// Settings that enable no built-in rules, and only the plugin `name`.
    fn plugin_settings(name: &str, options: Vec<(String, OptionValue)>) -> Result<LinterSettings> {
        let root = test_resource_path("fixtures/plugins");
        Ok(LinterSettings {
            plugins: PluginSet::load([PluginSpec {
                name: name.replace('_', "-"),
                path: root.join(format!("{name}.star")),
                root,
                options,
            }])?,
            ..settings::LinterSettings::for_rules([])
        })
    }

    #[test_case("api_migration", "api_migration.py")]
    #[test_case("async_blocking", "async_blocking.py")]
    #[test_case("banned_api", "banned_api.py")]
    #[test_case("django_models", "django_models.py")]
    #[test_case("edits", "edits.py")]
    #[test_case("env_access", "env_access.py")]
    #[test_case("python_ast", "python_ast.py")]
    #[test_case("slop", "slop.py")]
    #[test_case("starlark_functions", "starlark_functions.py")]
    #[test_case("structured_logging", "structured_logging.py")]
    #[test_case("test_hygiene", "tests/test_example.py")]
    #[test_case("test_hygiene", "not_a_test.py")]
    #[test_case("routes", "routes/urls.py")]
    #[test_case("todo_tickets", "todo_tickets.py")]
    fn plugin(name: &str, path: &str) -> Result<()> {
        let snapshot = format!("plugin_{name}__{}", path.replace('/', "__"));
        let diagnostics = test_path(
            Path::new("plugins").join(path),
            &plugin_settings(name, Vec::new())?,
        )?;
        assert_diagnostics!(snapshot, diagnostics);
        Ok(())
    }

    #[test]
    fn plugin_feature_flags() -> Result<()> {
        let settings = plugin_settings(
            "feature_flags",
            vec![
                ("launched".to_string(), strings(&["new-checkout"])),
                ("removed".to_string(), strings(&["legacy-search"])),
            ],
        )?;
        let diagnostics = test_path(Path::new("plugins/feature_flags.py"), &settings)?;
        assert_diagnostics!(diagnostics);
        Ok(())
    }

    #[test_case("layers/app/domain/model.py")]
    #[test_case("layers/app/api/views.py")]
    #[test_case("layers/script.py")]
    fn plugin_layers(path: &str) -> Result<()> {
        let settings = plugin_settings(
            "layers",
            vec![(
                "layers".to_string(),
                strings(&["app.api", "app.services", "app.domain"]),
            )],
        )?;
        let snapshot = format!("plugin_layers__{}", path.replace('/', "__"));
        let diagnostics = test_path(Path::new("plugins").join(path), &settings)?;
        assert_diagnostics!(snapshot, diagnostics);
        Ok(())
    }
}
