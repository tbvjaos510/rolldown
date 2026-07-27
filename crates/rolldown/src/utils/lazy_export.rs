//! Lowering for modules whose exports are synthesized rather than written: JSON, text,
//! base64 and dataurl (`NormalModule::meta::has_lazy_export`). The scanner leaves such a
//! module as a single expression statement with `ExportsKind::None`; this turns it into
//! real export syntax.
//!
//! Both output paths need it and neither may disagree with the other: the link stage
//! lowers before scope hoisting, and the dev module-wrapper path lowers its own AST clone
//! (see `internal-docs/lazy-compilation/implementation.md`). Keeping the lowering here
//! rather than in either finalizer is what makes "a JSON module exports `default` plus one
//! name per key" a single statement instead of two.

use indexmap::map::Entry;
use oxc::allocator::GetAllocator;
use oxc::ast::builder::AstBuilder;
use oxc::{
  allocator::{ReplaceWith, TakeIn},
  ast::ast::{self, Expression, Statement},
  span::SPAN,
};
use oxc_str::CompactStr;
use rolldown_ecmascript::EcmaAst;
use rolldown_ecmascript_utils::{ExpressionFactoryExt as _, StatementFactoryExt as _};
use rolldown_utils::ecmascript::legitimize_json_local_binding_name;
use rolldown_utils::indexmap::FxIndexMap;

/// Local binding name -> (exported name, whether the exported name is a legal identifier).
pub type JsonBindings = FxIndexMap<CompactStr, (CompactStr, bool)>;

/// What a lazy-export module was lowered into. Callers that keep their own bookkeeping
/// (the link stage does) branch on this instead of re-deciding the shape themselves.
pub enum LoweredLazyExport {
  /// `module.exports = <value>`
  CommonJs,
  /// `export default <value>`
  EsmDefault,
  /// `var <key> = <value>; … export default { <key>, … }; export { <key>, … }`
  EsmJsonObject(JsonBindings),
}

#[derive(Clone, Copy)]
enum LazyExportWrap {
  CjsExport,
  EsmDefault,
}

/// Lower the single expression statement that makes up a lazy-export module.
///
/// `is_commonjs` is the module's *resolved* exports kind. Only the link stage knows it;
/// callers that run before linking pass `false` and get the ESM shape, which is what an
/// `import` of the module needs.
pub fn lower_lazy_export(
  ecma_ast: &mut EcmaAst,
  is_json: bool,
  is_commonjs: bool,
) -> LoweredLazyExport {
  if is_commonjs {
    replace_first_expr_stmt(ecma_ast, LazyExportWrap::CjsExport);
    return LoweredLazyExport::CommonJs;
  }

  // A JSON object literal becomes one binding per key, so an importer can name them and
  // unused ones can be dropped. Any other JSON payload (an array, a bare string) has no
  // keys to split, and falls through to the plain default export.
  if is_json && let Some(bindings) = split_json_object_module(ecma_ast) {
    return LoweredLazyExport::EsmJsonObject(bindings);
  }

  replace_first_expr_stmt(ecma_ast, LazyExportWrap::EsmDefault);
  LoweredLazyExport::EsmDefault
}

/// Takes the expression of the first statement (which must be an `ExpressionStatement`)
/// and replaces the statement with either `module.exports = expr` or `export default expr`.
fn replace_first_expr_stmt(ecma_ast: &mut EcmaAst, kind: LazyExportWrap) {
  ecma_ast.program.with_mut(|fields| {
    let ast_builder = AstBuilder::new(fields.allocator);
    let Some(stmt) = fields.program.body.first_mut() else { unreachable!() };
    stmt.replace_with(|old| {
      let ast::Statement::ExpressionStatement(expr_stmt) = old else { unreachable!() };
      let expr = expr_stmt.unbox().expression;
      match kind {
        LazyExportWrap::CjsExport => Statement::new_module_exports_stmt(expr, &ast_builder),
        LazyExportWrap::EsmDefault => Statement::new_export_default_stmt(expr, &ast_builder),
      }
    });
  });
}

/// Returns the owned expression with any wrapping `(...)` parentheses removed.
fn into_without_parentheses(mut expr: Expression<'_>) -> Expression<'_> {
  while let Expression::ParenthesizedExpression(paren_expr) = expr {
    expr = paren_expr.unbox().expression;
  }
  expr
}

/// Rewrite `({ "a": 1, "b": 2 })` into `var a = 1; var b = 2; export default { a, b };
/// export { a, b };`, returning the bindings it declared.
///
/// Returns `None` — leaving the AST untouched — when the payload is not an object literal.
fn split_json_object_module(ecma_ast: &mut EcmaAst) -> Option<JsonBindings> {
  let mut declaration_binding_names: JsonBindings = FxIndexMap::default();
  let transformed = ecma_ast.program.with_mut(|fields| {
    let mut index_map = FxIndexMap::default();
    let ast_builder = AstBuilder::new(fields.allocator);
    let program = fields.program;
    let Some(ast::Statement::ExpressionStatement(stmt)) = program.body.first() else {
      unreachable!()
    };
    if !matches!(stmt.expression.without_parentheses(), Expression::ObjectExpression(_)) {
      return false;
    }
    // Take the single-statement body by value; this leaves `program.body` empty.
    let Some(ast::Statement::ExpressionStatement(stmt)) =
      program.body.take_in(&ast_builder.allocator()).into_iter().next()
    else {
      unreachable!();
    };
    let Expression::ObjectExpression(mut obj_expr) =
      into_without_parentheses(stmt.unbox().expression)
    else {
      unreachable!();
    };

    // convert {"a": "b", "c": "d"} to
    // {"a": b, "c": d}
    // and collect related info
    for property in &mut obj_expr.properties {
      match property {
        ast::ObjectPropertyKind::ObjectProperty(property) => {
          let key = property.key.static_name().expect("should be static name");
          if key.is_empty() {
            continue;
          }
          let legitimized_ident =
            legitimize_json_local_binding_name(&key, &declaration_binding_names);

          let is_legal_ident = legitimized_ident.as_str() == key;

          declaration_binding_names
            .insert(legitimized_ident.clone(), (CompactStr::new(&key), is_legal_ident));

          let value = std::mem::replace(
            &mut property.value,
            Expression::new_id_ref_expr(SPAN, legitimized_ident.as_str(), &ast_builder),
          );
          // TODO(shulaoda): Waiting for oxc transform to support the ES feature `ShorthandProperties`.
          if key == "__proto__" {
            property.computed = true;
          } else if is_legal_ident {
            property.shorthand = is_legal_ident;
            property.key = ast::PropertyKey::new_static_identifier(
              SPAN,
              oxc::ast::ast::Str::from_str_in(legitimized_ident.as_ref(), &ast_builder),
              &ast_builder,
            );
          }
          match index_map.entry(legitimized_ident) {
            Entry::Occupied(mut occ) => {
              *occ.get_mut() = value;
            }
            Entry::Vacant(vac) => {
              vac.insert(value);
            }
          }
        }
        ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
      }
    }
    // recreate Json Module
    let stmts = index_map
      .into_iter()
      // declaration
      .map(|(local, v)| Statement::new_var_decl(local.as_str(), v, &ast_builder))
      // export default json module
      .chain(std::iter::once(Statement::new_export_default_stmt(
        Expression::ObjectExpression(obj_expr),
        &ast_builder,
      )))
      // export all declaration
      .chain(std::iter::once(Statement::new_export_named_stmt(
        None,
        declaration_binding_names.iter(),
        &ast_builder,
      )));
    program.body.extend(stmts);
    true
  });

  transformed.then_some(declaration_binding_names)
}
