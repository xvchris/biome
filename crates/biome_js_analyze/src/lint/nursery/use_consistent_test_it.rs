use biome_analyze::{
    Ast, FixKind, Rule, RuleDiagnostic, RuleDomain, RuleSource, context::RuleContext,
    declare_lint_rule,
};
use biome_console::markup;
use biome_diagnostics::Severity;
use biome_js_factory::make;
use biome_js_syntax::{AnyJsExpression, AnyJsName, JsCallExpression, JsLanguage, JsSyntaxToken, T};
use biome_rowan::{AstNode, BatchMutation, BatchMutationExt, TextRange, TokenText};
use biome_rule_options::use_consistent_test_it::{TestFunctionKind, UseConsistentTestItOptions};

use crate::JsRuleAction;

declare_lint_rule! {
    /// Enforce consistent use of `it` or `test` for test functions.
    ///
    /// `it` and `test` are aliases for the same function in most test frameworks.
    /// This rule enforces using one over the other for consistency.
    ///
    ///
    /// ## Examples
    ///
    /// ### Invalid (default options: `function: "it"`)
    ///
    /// ```js,expect_diagnostic
    /// test("foo", () => {});
    /// ```
    ///
    /// ### Valid
    ///
    /// ```js
    /// it("foo", () => {});
    /// ```
    ///
    /// ## Options
    ///
    /// ### `function`
    ///
    /// The function to use for top-level tests (outside `describe` blocks).
    /// Accepted values are:
    /// - `"it"` (default): Enforce using `it()` for top-level tests
    /// - `"test"`: Enforce using `test()` for top-level tests
    ///
    /// ### `withinDescribe`
    ///
    /// The function to use for tests inside `describe` blocks.
    /// Accepted values are:
    /// - `"it"` (default): Enforce using `it()` inside describe blocks
    /// - `"test"`: Enforce using `test()` inside describe blocks
    ///
    pub UseConsistentTestIt {
        version: "next",
        name: "useConsistentTestIt",
        language: "js",
        recommended: false,
        severity: Severity::Warning,
        sources: &[
            RuleSource::EslintJest("consistent-test-it").inspired(),
            RuleSource::EslintVitest("consistent-test-it").inspired(),
        ],
        fix_kind: FixKind::Safe,
        domains: &[RuleDomain::Test],
    }
}

pub struct ConsistentTestItState {
    /// The kind of rename to apply
    rename_kind: RenameKind,
    /// Range for the diagnostic (points to the base function name)
    range: TextRange,
}

/// Internal enum to track what kind of rename to apply
enum RenameKind {
    /// Rename `it` -> `test` (includes variants like `it.skip`, `it.only`, etc.)
    ItToTest,
    /// Rename `test` -> `it` (includes variants like `test.skip`, `test.only`, etc.)
    TestToIt,
    /// Rename `xit` -> `xtest`
    XitToXtest,
    /// Rename `xtest` -> `xit`
    XtestToXit,
    /// Rename `fit` -> `test.only`
    FitToTestOnly,
    /// Rename `fit` -> `it.only`
    FitToItOnly,
    /// Rename `test.only` -> `fit`
    TestOnlyToFit,
}

impl Rule for UseConsistentTestIt {
    type Query = Ast<JsCallExpression>;
    type State = ConsistentTestItState;
    type Signals = Option<Self::State>;
    type Options = UseConsistentTestItOptions;

    fn run(ctx: &RuleContext<Self>) -> Self::Signals {
        let node = ctx.query();
        let options = ctx.options();

        // Get the required function kind based on context (inside/outside describe)
        let within_describe = is_within_describe(node);
        let required_kind = if within_describe {
            options.within_describe()
        } else {
            options.function()
        };

        let callee = node.callee().ok()?;

        // Get the base identifier name (it, test, xit, xtest, fit)
        let (base_name, base_token) = get_test_base_name(&callee)?;

        let rename_kind = match (base_name.text(), required_kind) {
            // `it` when `test` is required
            ("it", TestFunctionKind::Test) => RenameKind::ItToTest,
            // `test` when `it` is required
            ("test", TestFunctionKind::It) => RenameKind::TestToIt,
            // `xit` when `test` is required (becomes `xtest`)
            ("xit", TestFunctionKind::Test) => RenameKind::XitToXtest,
            // `xtest` when `it` is required (becomes `xit`)
            ("xtest", TestFunctionKind::It) => RenameKind::XtestToXit,
            // `fit` when `test` is required (becomes `test.only`)
            ("fit", TestFunctionKind::Test) => RenameKind::FitToTestOnly,
            // `fit` when `it` is required (becomes `it.only`)
            ("fit", TestFunctionKind::It) => RenameKind::FitToItOnly,
            // `test.only` when `it` is required (becomes `fit`)
            _ => {
                if required_kind == TestFunctionKind::It
                    && base_name.text() == "test"
                    && is_static_member_only(&callee)
                {
                    RenameKind::TestOnlyToFit
                } else {
                    return None;
                }
            }
        };

        Some(ConsistentTestItState {
            rename_kind,
            range: base_token.text_trimmed_range(),
        })
    }

    fn diagnostic(_ctx: &RuleContext<Self>, state: &Self::State) -> Option<RuleDiagnostic> {
        let (current, suggested) = match state.rename_kind {
            RenameKind::ItToTest => ("it", "test"),
            RenameKind::TestToIt => ("test", "it"),
            RenameKind::XitToXtest => ("xit", "xtest"),
            RenameKind::XtestToXit => ("xtest", "xit"),
            RenameKind::FitToTestOnly => ("fit", "test.only"),
            RenameKind::FitToItOnly => ("fit", "it.only"),
            RenameKind::TestOnlyToFit => ("test.only", "fit"),
        };

        Some(
            RuleDiagnostic::new(
                rule_category!(),
                state.range,
                markup! {
                    "Prefer using "<Emphasis>{suggested}</Emphasis>" over "<Emphasis>{current}</Emphasis>" for test functions."
                },
            )
            .note(markup! {
                "Use "<Emphasis>{suggested}</Emphasis>" consistently for all test function calls."
            }),
        )
    }

    fn action(ctx: &RuleContext<Self>, state: &Self::State) -> Option<JsRuleAction> {
        let node = ctx.query();
        let callee = node.callee().ok()?;
        let mut mutation = ctx.root().begin();

        let message = match state.rename_kind {
            RenameKind::ItToTest => {
                rename_base_identifier(&callee, "test", &mut mutation)?;
                markup! { "Use "<Emphasis>"test"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::TestToIt => {
                rename_base_identifier(&callee, "it", &mut mutation)?;
                markup! { "Use "<Emphasis>"it"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::XitToXtest => {
                rename_base_identifier(&callee, "xtest", &mut mutation)?;
                markup! { "Use "<Emphasis>"xtest"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::XtestToXit => {
                rename_base_identifier(&callee, "xit", &mut mutation)?;
                markup! { "Use "<Emphasis>"xit"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::FitToTestOnly => {
                fix_fit_to_test_only(&callee, &mut mutation)?;
                markup! { "Use "<Emphasis>"test.only"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::FitToItOnly => {
                fix_fit_to_it_only(&callee, &mut mutation)?;
                markup! { "Use "<Emphasis>"it.only"</Emphasis>" instead." }.to_owned()
            }
            RenameKind::TestOnlyToFit => {
                fix_test_only_to_fit(&callee, &mut mutation)?;
                markup! { "Use "<Emphasis>"fit"</Emphasis>" instead." }.to_owned()
            }
        };

        Some(JsRuleAction::new(
            ctx.metadata().action_category(ctx.category(), ctx.group()),
            ctx.metadata().applicability(),
            message,
            mutation,
        ))
    }
}

/// Get the base name token of a test callee expression.
///
/// Returns `(text, token)` where `text` is the base identifier name
/// (`it`, `test`, `xit`, `xtest`, or `fit`) and `token` is the syntax token.
///
/// Examples:
/// - `it(...)` → `("it", <it token>)`
/// - `it.only(...)` → `("it", <it token>)`
/// - `it.skip.each([])()` → `("it", <it token>)`
/// - `xit(...)` → `("xit", <xit token>)`
/// - `fit(...)` → `("fit", <fit token>)`
fn get_test_base_name(callee: &AnyJsExpression) -> Option<(TokenText, JsSyntaxToken)> {
    let base = get_base_identifier(callee)?;
    let text = base.token_text_trimmed();
    match text.text() {
        "it" | "test" | "xit" | "xtest" | "fit" => Some((text, base)),
        _ => None,
    }
}

/// Recursively get the base identifier token from a callee expression chain.
///
/// For `it` → returns the `it` token
/// For `it.only` → returns the `it` token
/// For `it.only.each` → returns the `it` token
fn get_base_identifier(callee: &AnyJsExpression) -> Option<JsSyntaxToken> {
    match callee {
        AnyJsExpression::JsIdentifierExpression(id) => id.name().ok()?.value_token().ok(),
        AnyJsExpression::JsStaticMemberExpression(member) => {
            let obj = member.object().ok()?;
            get_base_identifier(&obj)
        }
        AnyJsExpression::JsTemplateExpression(tmpl) => {
            // For tagged template expressions like `it.each`...``()
            let tag = tmpl.tag()?;
            get_base_identifier(&tag)
        }
        _ => None,
    }
}

/// Check if the callee is a `<base>.only` static member expression.
///
/// Used to detect `test.only` before converting to `fit`.
fn is_static_member_only(callee: &AnyJsExpression) -> bool {
    if let AnyJsExpression::JsStaticMemberExpression(member) = callee
        && let Ok(AnyJsName::JsName(name)) = member.member()
        && let Ok(token) = name.value_token()
    {
        return token.text_trimmed() == "only";
    }
    false
}

/// Rename the base identifier in a callee expression to a new name.
///
/// For `it(...)` → renames `it` to `test`
/// For `it.only(...)` → renames `it` to `test` (producing `test.only(...)`)
fn rename_base_identifier(
    callee: &AnyJsExpression,
    new_name: &str,
    mutation: &mut BatchMutation<JsLanguage>,
) -> Option<()> {
    let base = get_base_identifier(callee)?;
    let new_ref = make::js_reference_identifier(make::ident(new_name));
    mutation.replace_element(base.into(), new_ref.into());
    Some(())
}

/// Fix `fit(...)` → `test.only(...)`
///
/// Replaces the `fit` identifier with a `test.only` static member expression.
/// For `fit.skip(...)` → produces `test.only.skip(...)`.
fn fix_fit_to_test_only(
    callee: &AnyJsExpression,
    mutation: &mut BatchMutation<JsLanguage>,
) -> Option<()> {
    // Build `test.only` as a static member expression
    let test_ref =
        make::js_identifier_expression(make::js_reference_identifier(make::ident("test")));
    let test_only = make::js_static_member_expression(
        AnyJsExpression::JsIdentifierExpression(test_ref),
        make::token(T![.]),
        AnyJsName::JsName(make::js_name(make::ident("only"))),
    );

    match callee {
        AnyJsExpression::JsIdentifierExpression(_) => {
            // `fit(...)` → replace the whole callee with `test.only`
            mutation.replace_node(
                callee.clone(),
                AnyJsExpression::JsStaticMemberExpression(test_only),
            );
        }
        AnyJsExpression::JsStaticMemberExpression(member) => {
            // `fit.something(...)` → replace the object `fit` with `test.only`
            let obj = member.object().ok()?;
            mutation.replace_node(obj, AnyJsExpression::JsStaticMemberExpression(test_only));
        }
        _ => return None,
    }
    Some(())
}

/// Fix `fit(...)` → `it.only(...)`
///
/// Replaces the `fit` identifier with an `it.only` static member expression.
/// For `fit.skip(...)` → produces `it.only.skip(...)`.
fn fix_fit_to_it_only(
    callee: &AnyJsExpression,
    mutation: &mut BatchMutation<JsLanguage>,
) -> Option<()> {
    // Build `it.only` as a static member expression
    let it_ref = make::js_identifier_expression(make::js_reference_identifier(make::ident("it")));
    let it_only = make::js_static_member_expression(
        AnyJsExpression::JsIdentifierExpression(it_ref),
        make::token(T![.]),
        AnyJsName::JsName(make::js_name(make::ident("only"))),
    );

    match callee {
        AnyJsExpression::JsIdentifierExpression(_) => {
            // `fit(...)` → replace the whole callee with `it.only`
            mutation.replace_node(
                callee.clone(),
                AnyJsExpression::JsStaticMemberExpression(it_only),
            );
        }
        AnyJsExpression::JsStaticMemberExpression(member) => {
            // `fit.something(...)` → replace the object `fit` with `it.only`
            let obj = member.object().ok()?;
            mutation.replace_node(obj, AnyJsExpression::JsStaticMemberExpression(it_only));
        }
        _ => return None,
    }
    Some(())
}

/// Fix `test.only(...)` → `fit(...)`
///
/// Replaces the static member expression `test.only` with just `fit`.
fn fix_test_only_to_fit(
    callee: &AnyJsExpression,
    mutation: &mut BatchMutation<JsLanguage>,
) -> Option<()> {
    if let AnyJsExpression::JsStaticMemberExpression(_) = callee {
        let fit_ref =
            make::js_identifier_expression(make::js_reference_identifier(make::ident("fit")));
        mutation.replace_node(
            callee.clone(),
            AnyJsExpression::JsIdentifierExpression(fit_ref),
        );
        Some(())
    } else {
        None
    }
}

/// Check if a call expression is nested inside a `describe` block.
///
/// Walks up ancestors looking for a `JsCallExpression` whose callee starts with `describe`.
fn is_within_describe(node: &JsCallExpression) -> bool {
    node.syntax()
        .ancestors()
        .skip(1)
        .filter_map(JsCallExpression::cast)
        .any(|ancestor| {
            ancestor
                .callee()
                .ok()
                .is_some_and(|callee| callee.contains_describe_call())
        })
}
