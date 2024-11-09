use std::path::Path;

use itertools::Itertools;
use oxc::ast::VisitMut;
use oxc::diagnostics::{OxcDiagnostic, Severity as OxcSeverity};
use oxc::minifier::{CompressOptions, Compressor};
use oxc::semantic::{ScopeTree, SemanticBuilder, Stats, SymbolTable};
use oxc::transformer::{
  ESTarget as OxcESTarget, InjectGlobalVariables, ReplaceGlobalDefines, ReplaceGlobalDefinesConfig,
  TransformOptions, Transformer,
};

use rolldown_common::{ESTarget, NormalizedBundlerOptions};
use rolldown_ecmascript::{EcmaAst, WithMutFields};
use rolldown_error::{BuildDiagnostic, Severity};

use crate::types::oxc_parse_type::OxcParseType;

use super::ecma_visitors::EnsureSpanUniqueness;
use super::tweak_ast_for_scanning::PreProcessor;

#[derive(Default)]
pub struct PreProcessEcmaAst {
  /// Only recreate semantic data if ast is changed.
  ast_changed: bool,

  /// Semantic statistics.
  stats: Stats,
}

impl PreProcessEcmaAst {
  // #[allow(clippy::match_same_arms)]: `OxcParseType::Tsx` will have special logic to deal with ts compared to `OxcParseType::Jsx`
  #[allow(clippy::match_same_arms)]
  pub fn build(
    &mut self,
    mut ast: EcmaAst,
    parse_type: &OxcParseType,
    path: &str,
    replace_global_define_config: Option<&ReplaceGlobalDefinesConfig>,
    bundle_options: &NormalizedBundlerOptions,
    has_lazy_export: bool,
  ) -> anyhow::Result<(EcmaAst, SymbolTable, ScopeTree, Vec<BuildDiagnostic>), Vec<OxcDiagnostic>>
  {
    let mut warning = vec![];
    let source = ast.source().clone();
    // Build initial semantic data and check for semantic errors.
    let semantic_ret =
      ast.program.with_mut(|WithMutFields { program, .. }| SemanticBuilder::new().build(program));
    if !semantic_ret.errors.is_empty() {
      warning.extend(BuildDiagnostic::from_oxc_diagnostics(
        semantic_ret.errors,
        &source,
        path,
        &Severity::Warning,
      ));
    }

    self.stats = semantic_ret.semantic.stats();
    let (mut symbols, mut scopes) = semantic_ret.semantic.into_symbol_table_and_scope_tree();

    // Transform TypeScript and jsx.
    if !matches!(parse_type, OxcParseType::Js) || !matches!(bundle_options.target, ESTarget::EsNext)
    {
      let ret = ast.program.with_mut(move |fields| {
        let target: OxcESTarget = bundle_options.target.into();
        let mut transformer_options = TransformOptions::from(target);
        match parse_type {
          OxcParseType::Js => {}
          OxcParseType::Jsx | OxcParseType::Tsx => {
            transformer_options.jsx.jsx_plugin = true;
          }
          OxcParseType::Ts => {}
        }
        if let Some(jsx) = &bundle_options.jsx {
          transformer_options.jsx = jsx.clone();
        }

        Transformer::new(fields.allocator, Path::new(path), transformer_options)
          .build_with_symbols_and_scopes(symbols, scopes, fields.program)
      });

      // TODO: emit diagnostic, aiming to pass more tests,
      // we ignore warning for now
      let errors = ret
        .errors
        .into_iter()
        .filter(|item| matches!(item.severity, OxcSeverity::Error))
        .collect_vec();
      if !errors.is_empty() {
        return Err(errors);
      }

      symbols = ret.symbols;
      scopes = ret.scopes;
      self.ast_changed = true;
    }

    ast.program.with_mut(|fields| -> anyhow::Result<(), Vec<OxcDiagnostic>> {
      let WithMutFields { allocator, program, .. } = fields;
      // Use built-in define plugin.
      if let Some(replace_global_define_config) = replace_global_define_config {
        let ret = ReplaceGlobalDefines::new(allocator, replace_global_define_config.clone())
          .build(symbols, scopes, program);
        symbols = ret.symbols;
        scopes = ret.scopes;
        self.ast_changed = true;
      }
      if !bundle_options.inject.is_empty() {
        // if the define replace something, we need to recreate the semantic data.
        // to correct the `root_unresolved_references`
        // https://github.com/oxc-project/oxc/blob/0136431b31a1d4cc20147eb085d9314b224cc092/crates/oxc_transformer/src/plugins/inject_global_variables.rs#L184-L184
        // TODO: real ast_changed hint https://github.com/oxc-project/oxc/pull/7205
        let semantic_ret = SemanticBuilder::new().with_stats(self.stats).build(program);
        (symbols, scopes) = semantic_ret.semantic.into_symbol_table_and_scope_tree();
        let ret = InjectGlobalVariables::new(
          allocator,
          bundle_options.oxc_inject_global_variables_config.clone(),
        )
        .build(symbols, scopes, program);
        symbols = ret.symbols;
        scopes = ret.scopes;
        self.ast_changed = true;
      }

      // avoid DCE for lazy export
      if bundle_options.treeshake.enabled() && !has_lazy_export {
        // Perform dead code elimination.
        // NOTE: `CompressOptions::dead_code_elimination` will remove `ParenthesizedExpression`s from the AST.
        let compressor = Compressor::new(allocator, CompressOptions::dead_code_elimination());
        if self.ast_changed {
          let semantic_ret = SemanticBuilder::new().with_stats(self.stats).build(program);
          (symbols, scopes) = semantic_ret.semantic.into_symbol_table_and_scope_tree();
        }
        compressor.build_with_symbols_and_scopes(symbols, scopes, program);
      }

      Ok(())
    })?;

    ast.program.with_mut(|fields| {
      let mut pre_processor = PreProcessor::new(fields.allocator);
      pre_processor.visit_program(fields.program);
      ast.contains_use_strict = pre_processor.contains_use_strict;
    });

    ast.program.with_mut(|fields| {
      EnsureSpanUniqueness::new().visit_program(fields.program);
    });
    // NOTE: Recreate semantic data because AST is changed in the transformations above.
    (symbols, scopes) = ast.program.with_dependent(|_owner, dep| {
      SemanticBuilder::new()
        // Required by `module.scope.get_child_ids` in `crates/rolldown/src/utils/renamer.rs`.
        .with_scope_tree_child_ids(true)
        // Preallocate memory for the underlying data structures.
        .with_stats(self.stats)
        .build(&dep.program)
        .semantic
        .into_symbol_table_and_scope_tree()
    });

    Ok((ast, symbols, scopes, warning))
  }
}
