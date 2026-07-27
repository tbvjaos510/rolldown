use oxc::{
  semantic::{SemanticBuilder, Stats},
  span::SPAN,
};
use rolldown_common::{
  EcmaModuleAstUsage, ExportsKind, GetLocalDbMut, LocalExport, ModuleIdx, ModuleType, NormalModule,
  StmtInfo, StmtInfoIdx, StmtInfos, SymbolOrMemberExprRef, SymbolRef, SymbolRefDbForModule,
  TaggedSymbolRef, WrapKind,
};
#[cfg(not(target_family = "wasm"))]
use rolldown_utils::rayon::IndexedParallelIterator;
use rolldown_utils::rayon::{IntoParallelRefMutIterator, ParallelIterator};
use smallvec::smallvec;

use crate::utils::lazy_export::{JsonBindings, LoweredLazyExport, lower_lazy_export};

use super::LinkStage;

/// Index of the first statement after the namespace statement (index 0).
const FIRST_TOP_LEVEL_STMT_IDX: StmtInfoIdx = StmtInfoIdx::from_raw_unchecked(1);

struct LazyModuleInfo {
  idx: ModuleIdx,
  exports_kind: ExportsKind,
  is_json: bool,
}

impl LinkStage<'_> {
  #[tracing::instrument(level = "debug", skip_all)]
  pub(super) fn generate_lazy_export(&mut self) {
    let lazy_modules = append_only_vec::AppendOnlyVec::new();
    self
      .module_table
      .modules
      .par_iter_mut()
      .zip(self.stmt_infos.par_iter_mut())
      .filter_map(|(m, stmt_infos)| m.as_normal_mut().map(|m| (m, stmt_infos)))
      .filter(|(module, _)| module.meta.has_lazy_export())
      .for_each(|(module, stmt_infos)| {
        let default_symbol_ref = module.default_export_ref;
        let is_json = matches!(module.module_type, ModuleType::Json);
        if !is_json || module.exports_kind == ExportsKind::CommonJs {
          update_module_default_export_info(module, stmt_infos, default_symbol_ref);
        }
        lazy_modules.push(LazyModuleInfo {
          idx: module.idx,
          exports_kind: module.exports_kind,
          is_json,
        });

        // generate `module.exports = expr`
        if module.exports_kind == ExportsKind::CommonJs {
          // since the wrap arguments are generate on demand, we need to insert the module ref usage here.
          stmt_infos.infos[FIRST_TOP_LEVEL_STMT_IDX].eval_flags = true.into();
          module.ecma_view.ast_usage.insert(EcmaModuleAstUsage::ModuleRef);
        }
      });

    for LazyModuleInfo { idx: module_idx, exports_kind, is_json } in lazy_modules {
      let Some(ecma_ast) = &mut self.ast_table[module_idx] else { unreachable!() };
      // The AST rewrite is shared with the dev module-wrapper path, which has no link stage
      // to lower these modules for it; only the bookkeeping below is this stage's own.
      match lower_lazy_export(ecma_ast, is_json, matches!(exports_kind, ExportsKind::CommonJs)) {
        LoweredLazyExport::CommonJs => continue,
        LoweredLazyExport::EsmJsonObject(bindings) => {
          record_json_object_exports(self, module_idx, &bindings);
          continue;
        }
        LoweredLazyExport::EsmDefault => {
          // A JSON payload that turned out not to be an object literal deferred this until
          // now; every other lazy-export module already recorded it above.
          if is_json {
            let stmt_infos = &mut self.stmt_infos[module_idx];
            let module = self.module_table[module_idx].as_normal_mut().unwrap();
            let default_export_ref = module.default_export_ref;
            update_module_default_export_info(module, stmt_infos, default_export_ref);
          }
        }
      }

      // Ensure exports_kind is set to Esm for all modules that generate ESM export syntax.
      // This is needed for proper CJS export rendering in preserveModules mode.
      let module = &mut self.module_table[module_idx];
      let module = module.as_normal_mut().unwrap();
      module.exports_kind = ExportsKind::Esm;
    }
  }
}

fn update_module_default_export_info(
  module: &mut NormalModule,
  stmt_infos: &mut StmtInfos,
  default_symbol_ref: SymbolRef,
) {
  module.named_exports.insert(
    "default".into(),
    LocalExport { span: SPAN, referenced: default_symbol_ref, came_from_commonjs: false },
  );
  stmt_infos
    .declare_symbol_for_stmt(FIRST_TOP_LEVEL_STMT_IDX, TaggedSymbolRef::normal(default_symbol_ref));
}

/// Bookkeeping for a JSON module that `lower_lazy_export` split into one binding per key:
/// semantic data for the rewritten AST, a named export per binding, and the statement infos
/// that let tree shaking drop the keys nobody imported.
fn record_json_object_exports(
  link_staged: &mut LinkStage,
  module_idx: ModuleIdx,
  declaration_binding_names: &JsonBindings,
) {
  let original_symbol_ref_db = std::mem::take(link_staged.symbols.local_db_mut(module_idx));
  // recreate semantic data
  #[expect(clippy::cast_possible_truncation)]
  let scoping = {
    let ecma_ast = link_staged.ast_table[module_idx].as_mut().unwrap();
    ecma_ast.make_symbol_table_and_scope_tree_with_semantic_builder(
      SemanticBuilder::new().with_stats(Stats {
        nodes: declaration_binding_names.len().next_power_of_two() as u32,
        scopes: 1,
        symbols: declaration_binding_names.len() as u32,
        references: declaration_binding_names.len() as u32 * 2u32,
      }),
    )
  };

  // update semantic data of module
  let root_scope_id = scoping.root_scope_id();
  let mut symbol_ref_db = SymbolRefDbForModule::new(scoping, module_idx, root_scope_id);
  let module = link_staged.module_table[module_idx].as_normal_mut().unwrap();
  // Re-create facade symbols in the new scoping. The JSON module was re-parsed above,
  // producing a new Scoping with fresh symbol IDs, so old facade IDs are invalid.
  // We allocate new IDs by name and update the module's references.
  let mut recreate_facade = |old_ref: SymbolRef| -> SymbolRef {
    symbol_ref_db.create_facade_root_symbol_ref(original_symbol_ref_db.symbol_name(old_ref.symbol))
  };
  module.namespace_object_ref = recreate_facade(module.namespace_object_ref);
  module.default_export_ref = recreate_facade(module.default_export_ref);
  if let Some(hot_ref) = module.hmr_hot_ref {
    module.hmr_hot_ref = Some(recreate_facade(hot_ref));
  }
  let namespace_object_ref = module.namespace_object_ref;
  let default_export_ref = module.default_export_ref;

  // update module stmts info
  // clear stmt info, since we need to split `ObjectExpression` into multiple decl, the original stmt info is invalid.
  // preserve the first one, which is `NamespaceRef`
  let stmt_infos = &mut link_staged.stmt_infos[module_idx];
  let stmt_info = stmt_infos.drain(FIRST_TOP_LEVEL_STMT_IDX..);
  let mut all_declared_symbols =
    stmt_info.flat_map(|info| info.referenced_symbols).collect::<Vec<_>>();
  for (local, (exported, _)) in declaration_binding_names {
    let symbol_id =
      symbol_ref_db.scoping().get_root_binding(local.as_str().into()).expect("should have binding");
    let symbol_ref: SymbolRef = (module_idx, symbol_id).into();
    all_declared_symbols.push(SymbolOrMemberExprRef::from(symbol_ref));
    let stmt_info =
      StmtInfo::default().with_declared_symbols(smallvec![TaggedSymbolRef::normal(symbol_ref)]);
    stmt_infos.add_stmt_info(stmt_info);
    module.named_exports.insert(
      exported.clone(),
      LocalExport { span: SPAN, referenced: symbol_ref, came_from_commonjs: false },
    );
  }
  // declare default export statement
  let stmt_info = StmtInfo::default()
    .with_declared_symbols(smallvec![TaggedSymbolRef::normal(default_export_ref)])
    .with_referenced_symbols(all_declared_symbols.clone());

  stmt_infos.add_stmt_info(stmt_info);
  module.named_exports.insert(
    "default".into(),
    LocalExport { span: SPAN, referenced: default_export_ref, came_from_commonjs: false },
  );

  // declare namespace object statement
  module.exports_kind = ExportsKind::Esm;
  stmt_infos.replace_namespace_stmt_info(
    StmtInfo::default()
      .with_declared_symbols(smallvec![TaggedSymbolRef::normal(namespace_object_ref)])
      .with_referenced_symbols(all_declared_symbols),
  );
  // for a es json module it did not needs to be wrapped anyway.
  link_staged.metas[module_idx].wrapper_stmt_info = None;
  link_staged.metas[module_idx].wrapper_ref = None;
  link_staged.metas[module_idx].set_wrap_kind(WrapKind::None);

  link_staged.symbols.store_local_db(module_idx, symbol_ref_db);
}
