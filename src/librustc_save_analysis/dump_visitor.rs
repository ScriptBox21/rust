//! Write the output of rustc's analysis to an implementor of Dump.
//!
//! Dumping the analysis is implemented by walking the AST and getting a bunch of
//! info out from all over the place. We use `DefId`s to identify objects. The
//! tricky part is getting syntactic (span, source text) and semantic (reference
//! `DefId`s) information for parts of expressions which the compiler has discarded.
//! E.g., in a path `foo::bar::baz`, the compiler only keeps a span for the whole
//! path and a reference to `baz`, but we want spans and references for all three
//! idents.
//!
//! SpanUtils is used to manipulate spans. In particular, to extract sub-spans
//! from spans (e.g., the span for `bar` from the above example path).
//! DumpVisitor walks the AST and processes it, and Dumper is used for
//! recording the output.

use rustc_ast::ast::{self};
use rustc_ast::{token, walk_list};
use rustc_data_structures::fx::FxHashSet;
use rustc_hir as hir;
use rustc_hir::def::{DefKind as HirDefKind, Res};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_hir::intravisit::{self, Visitor};
use rustc_hir_pretty::{bounds_to_string, generic_params_to_string, ty_to_string};
use rustc_middle::hir::map::Map;
use rustc_middle::span_bug;
use rustc_middle::ty::{self, DefIdTree, TyCtxt};
use rustc_session::config::Input;
use rustc_span::source_map::respan;
use rustc_span::symbol::Ident;
use rustc_span::*;

use std::env;
use std::path::Path;

use crate::dumper::{Access, Dumper};
use crate::sig;
use crate::span_utils::SpanUtils;
use crate::{
    escape, generated_code, id_from_def_id, id_from_hir_id, lower_attributes, PathCollector,
    SaveContext,
};

use rls_data::{
    CompilationOptions, CratePreludeData, Def, DefKind, GlobalCrateId, Import, ImportKind, Ref,
    RefKind, Relation, RelationKind, SpanData,
};

use log::{debug, error};

macro_rules! down_cast_data {
    ($id:ident, $kind:ident, $sp:expr) => {
        let $id = if let super::Data::$kind(data) = $id {
            data
        } else {
            span_bug!($sp, "unexpected data kind: {:?}", $id);
        };
    };
}

macro_rules! access_from {
    ($save_ctxt:expr, $item:expr, $id:expr) => {
        Access {
            public: $item.vis.node.is_pub(),
            reachable: $save_ctxt.access_levels.is_reachable($id),
        }
    };
}

macro_rules! access_from_vis {
    ($save_ctxt:expr, $vis:expr, $id:expr) => {
        Access { public: $vis.node.is_pub(), reachable: $save_ctxt.access_levels.is_reachable($id) }
    };
}

pub struct DumpVisitor<'l, 'tcx> {
    pub save_ctxt: SaveContext<'l, 'tcx>,
    tcx: TyCtxt<'tcx>,
    dumper: Dumper,

    span: SpanUtils<'l>,
    // Set of macro definition (callee) spans, and the set
    // of macro use (callsite) spans. We store these to ensure
    // we only write one macro def per unique macro definition, and
    // one macro use per unique callsite span.
    // mac_defs: FxHashSet<Span>,
    // macro_calls: FxHashSet<Span>,
}

impl<'l, 'tcx> DumpVisitor<'l, 'tcx> {
    pub fn new(save_ctxt: SaveContext<'l, 'tcx>) -> DumpVisitor<'l, 'tcx> {
        let span_utils = SpanUtils::new(&save_ctxt.tcx.sess);
        let dumper = Dumper::new(save_ctxt.config.clone());
        DumpVisitor {
            tcx: save_ctxt.tcx,
            save_ctxt,
            dumper,
            span: span_utils,
            // mac_defs: FxHashSet::default(),
            // macro_calls: FxHashSet::default(),
        }
    }

    pub fn analysis(&self) -> &rls_data::Analysis {
        self.dumper.analysis()
    }

    fn nest_tables<F>(&mut self, item_def_id: LocalDefId, f: F)
    where
        F: FnOnce(&mut Self),
    {
        let tables = if self.tcx.has_typeck_tables(item_def_id) {
            self.tcx.typeck_tables_of(item_def_id)
        } else {
            self.save_ctxt.empty_tables
        };

        let old_tables = self.save_ctxt.tables;
        self.save_ctxt.tables = tables;
        f(self);
        self.save_ctxt.tables = old_tables;
    }

    fn span_from_span(&self, span: Span) -> SpanData {
        self.save_ctxt.span_from_span(span)
    }

    fn lookup_def_id(&self, ref_id: hir::HirId) -> Option<DefId> {
        self.save_ctxt.lookup_def_id(ref_id)
    }

    pub fn dump_crate_info(&mut self, name: &str, krate: &hir::Crate<'_>) {
        let source_file = self.tcx.sess.local_crate_source_file.as_ref();
        let crate_root = source_file.map(|source_file| {
            let source_file = Path::new(source_file);
            match source_file.file_name() {
                Some(_) => source_file.parent().unwrap().display(),
                None => source_file.display(),
            }
            .to_string()
        });

        let data = CratePreludeData {
            crate_id: GlobalCrateId {
                name: name.into(),
                disambiguator: self
                    .tcx
                    .sess
                    .local_crate_disambiguator()
                    .to_fingerprint()
                    .as_value(),
            },
            crate_root: crate_root.unwrap_or_else(|| "<no source>".to_owned()),
            external_crates: self.save_ctxt.get_external_crates(),
            span: self.span_from_span(krate.item.span),
        };

        self.dumper.crate_prelude(data);
    }

    pub fn dump_compilation_options(&mut self, input: &Input, crate_name: &str) {
        // Apply possible `remap-path-prefix` remapping to the input source file
        // (and don't include remapping args anymore)
        let (program, arguments) = {
            let remap_arg_indices = {
                let mut indices = FxHashSet::default();
                // Args are guaranteed to be valid UTF-8 (checked early)
                for (i, e) in env::args().enumerate() {
                    if e.starts_with("--remap-path-prefix=") {
                        indices.insert(i);
                    } else if e == "--remap-path-prefix" {
                        indices.insert(i);
                        indices.insert(i + 1);
                    }
                }
                indices
            };

            let mut args = env::args()
                .enumerate()
                .filter(|(i, _)| !remap_arg_indices.contains(i))
                .map(|(_, arg)| match input {
                    Input::File(ref path) if path == Path::new(&arg) => {
                        let mapped = &self.tcx.sess.local_crate_source_file;
                        mapped.as_ref().unwrap().to_string_lossy().into()
                    }
                    _ => arg,
                });

            (args.next().unwrap(), args.collect())
        };

        let data = CompilationOptions {
            directory: self.tcx.sess.working_dir.0.clone(),
            program,
            arguments,
            output: self.save_ctxt.compilation_output(crate_name),
        };

        self.dumper.compilation_opts(data);
    }

    fn write_sub_paths(&mut self, path: &'tcx hir::Path<'tcx>) {
        for seg in path.segments {
            if let Some(data) = self.save_ctxt.get_path_segment_data(seg) {
                self.dumper.dump_ref(data);
            }
        }
    }

    // As write_sub_paths, but does not process the last ident in the path (assuming it
    // will be processed elsewhere). See note on write_sub_paths about global.
    fn write_sub_paths_truncated(&mut self, path: &'tcx hir::Path<'tcx>) {
        if let [segments @ .., _] = path.segments {
            for seg in segments {
                if let Some(data) = self.save_ctxt.get_path_segment_data(seg) {
                    self.dumper.dump_ref(data);
                }
            }
        }
    }

    fn process_formals(&mut self, formals: &'tcx [hir::Param<'tcx>], qualname: &str) {
        for arg in formals {
            self.visit_pat(&arg.pat);
            let mut collector = PathCollector::new(self.tcx);
            collector.visit_pat(&arg.pat);

            for (hir_id, ident, ..) in collector.collected_idents {
                let typ = match self.save_ctxt.tables.node_type_opt(hir_id) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                if !self.span.filter_generated(ident.span) {
                    let id = id_from_hir_id(hir_id, &self.save_ctxt);
                    let span = self.span_from_span(ident.span);

                    self.dumper.dump_def(
                        &Access { public: false, reachable: false },
                        Def {
                            kind: DefKind::Local,
                            id,
                            span,
                            name: ident.to_string(),
                            qualname: format!("{}::{}", qualname, ident.to_string()),
                            value: typ,
                            parent: None,
                            children: vec![],
                            decl_id: None,
                            docs: String::new(),
                            sig: None,
                            attributes: vec![],
                        },
                    );
                }
            }
        }
    }

    fn process_method(
        &mut self,
        sig: &'tcx hir::FnSig<'tcx>,
        body: Option<hir::BodyId>,
        hir_id: hir::HirId,
        ident: Ident,
        generics: &'tcx hir::Generics<'tcx>,
        vis: &hir::Visibility<'tcx>,
        span: Span,
    ) {
        debug!("process_method: {}:{}", hir_id, ident);

        let map = &self.tcx.hir();
        self.nest_tables(map.local_def_id(hir_id), |v| {
            if let Some(mut method_data) = v.save_ctxt.get_method_data(hir_id, ident, span) {
                if let Some(body) = body {
                    v.process_formals(map.body(body).params, &method_data.qualname);
                }
                v.process_generic_params(&generics, &method_data.qualname, hir_id);

                method_data.value = crate::make_signature(&sig.decl, &generics);
                method_data.sig = sig::method_signature(hir_id, ident, generics, sig, &v.save_ctxt);

                v.dumper.dump_def(&access_from_vis!(v.save_ctxt, vis, hir_id), method_data);
            }

            // walk arg and return types
            for arg in sig.decl.inputs {
                v.visit_ty(arg);
            }

            if let hir::FnRetTy::Return(ref ret_ty) = sig.decl.output {
                v.visit_ty(ret_ty)
            }

            // walk the fn body
            if let Some(body) = body {
                v.visit_expr(&map.body(body).value);
            }
        });
    }

    fn process_struct_field_def(
        &mut self,
        field: &'tcx hir::StructField<'tcx>,
        parent_id: hir::HirId,
    ) {
        let field_data = self.save_ctxt.get_field_data(field, parent_id);
        if let Some(field_data) = field_data {
            self.dumper.dump_def(&access_from!(self.save_ctxt, field, field.hir_id), field_data);
        }
    }

    // Dump generic params bindings, then visit_generics
    fn process_generic_params(
        &mut self,
        generics: &'tcx hir::Generics<'tcx>,
        prefix: &str,
        id: hir::HirId,
    ) {
        for param in generics.params {
            match param.kind {
                hir::GenericParamKind::Lifetime { .. } => {}
                hir::GenericParamKind::Type { .. } => {
                    let param_ss = param.name.ident().span;
                    let name = escape(self.span.snippet(param_ss));
                    // Append $id to name to make sure each one is unique.
                    let qualname = format!("{}::{}${}", prefix, name, id);
                    if !self.span.filter_generated(param_ss) {
                        let id = id_from_hir_id(param.hir_id, &self.save_ctxt);
                        let span = self.span_from_span(param_ss);

                        self.dumper.dump_def(
                            &Access { public: false, reachable: false },
                            Def {
                                kind: DefKind::Type,
                                id,
                                span,
                                name,
                                qualname,
                                value: String::new(),
                                parent: None,
                                children: vec![],
                                decl_id: None,
                                docs: String::new(),
                                sig: None,
                                attributes: vec![],
                            },
                        );
                    }
                }
                hir::GenericParamKind::Const { .. } => {}
            }
        }
        self.visit_generics(generics);
    }

    fn process_fn(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        decl: &'tcx hir::FnDecl<'tcx>,
        _header: &'tcx hir::FnHeader,
        ty_params: &'tcx hir::Generics<'tcx>,
        body: hir::BodyId,
    ) {
        let map = &self.tcx.hir();
        self.nest_tables(map.local_def_id(item.hir_id), |v| {
            let body = map.body(body);
            if let Some(fn_data) = v.save_ctxt.get_item_data(item) {
                down_cast_data!(fn_data, DefData, item.span);
                v.process_formals(body.params, &fn_data.qualname);
                v.process_generic_params(ty_params, &fn_data.qualname, item.hir_id);

                v.dumper.dump_def(&access_from!(v.save_ctxt, item, item.hir_id), fn_data);
            }

            for arg in decl.inputs {
                v.visit_ty(arg)
            }

            if let hir::FnRetTy::Return(ref ret_ty) = decl.output {
                v.visit_ty(ret_ty)
            }

            v.visit_expr(&body.value);
        });
    }

    fn process_static_or_const_item(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        typ: &'tcx hir::Ty<'tcx>,
        expr: &'tcx hir::Expr<'tcx>,
    ) {
        self.nest_tables(self.tcx.hir().local_def_id(item.hir_id), |v| {
            if let Some(var_data) = v.save_ctxt.get_item_data(item) {
                down_cast_data!(var_data, DefData, item.span);
                v.dumper.dump_def(&access_from!(v.save_ctxt, item, item.hir_id), var_data);
            }
            v.visit_ty(&typ);
            v.visit_expr(expr);
        });
    }

    fn process_assoc_const(
        &mut self,
        hir_id: hir::HirId,
        ident: Ident,
        typ: &'tcx hir::Ty<'tcx>,
        expr: Option<&'tcx hir::Expr<'tcx>>,
        parent_id: DefId,
        vis: &hir::Visibility<'tcx>,
        attrs: &'tcx [ast::Attribute],
    ) {
        let qualname =
            format!("::{}", self.tcx.def_path_str(self.tcx.hir().local_def_id(hir_id).to_def_id()));

        if !self.span.filter_generated(ident.span) {
            let sig = sig::assoc_const_signature(hir_id, ident.name, typ, expr, &self.save_ctxt);
            let span = self.span_from_span(ident.span);

            self.dumper.dump_def(
                &access_from_vis!(self.save_ctxt, vis, hir_id),
                Def {
                    kind: DefKind::Const,
                    id: id_from_hir_id(hir_id, &self.save_ctxt),
                    span,
                    name: ident.name.to_string(),
                    qualname,
                    value: ty_to_string(&typ),
                    parent: Some(id_from_def_id(parent_id)),
                    children: vec![],
                    decl_id: None,
                    docs: self.save_ctxt.docs_for_attrs(attrs),
                    sig,
                    attributes: lower_attributes(attrs.to_owned(), &self.save_ctxt),
                },
            );
        }

        // walk type and init value
        self.nest_tables(self.tcx.hir().local_def_id(hir_id), |v| {
            v.visit_ty(typ);
            if let Some(expr) = expr {
                v.visit_expr(expr);
            }
        });
    }

    // FIXME tuple structs should generate tuple-specific data.
    fn process_struct(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        def: &'tcx hir::VariantData<'tcx>,
        ty_params: &'tcx hir::Generics<'tcx>,
    ) {
        debug!("process_struct {:?} {:?}", item, item.span);
        let name = item.ident.to_string();
        let qualname = format!(
            "::{}",
            self.tcx.def_path_str(self.tcx.hir().local_def_id(item.hir_id).to_def_id())
        );

        let kind = match item.kind {
            hir::ItemKind::Struct(_, _) => DefKind::Struct,
            hir::ItemKind::Union(_, _) => DefKind::Union,
            _ => unreachable!(),
        };

        let (value, fields) = match item.kind {
            hir::ItemKind::Struct(hir::VariantData::Struct(ref fields, ..), ..)
            | hir::ItemKind::Union(hir::VariantData::Struct(ref fields, ..), ..) => {
                let include_priv_fields = !self.save_ctxt.config.pub_only;
                let fields_str = fields
                    .iter()
                    .filter_map(|f| {
                        if include_priv_fields || f.vis.node.is_pub() {
                            Some(f.ident.to_string())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let value = format!("{} {{ {} }}", name, fields_str);
                (value, fields.iter().map(|f| id_from_hir_id(f.hir_id, &self.save_ctxt)).collect())
            }
            _ => (String::new(), vec![]),
        };

        if !self.span.filter_generated(item.ident.span) {
            let span = self.span_from_span(item.ident.span);
            self.dumper.dump_def(
                &access_from!(self.save_ctxt, item, item.hir_id),
                Def {
                    kind,
                    id: id_from_hir_id(item.hir_id, &self.save_ctxt),
                    span,
                    name,
                    qualname: qualname.clone(),
                    value,
                    parent: None,
                    children: fields,
                    decl_id: None,
                    docs: self.save_ctxt.docs_for_attrs(&item.attrs),
                    sig: sig::item_signature(item, &self.save_ctxt),
                    attributes: lower_attributes(item.attrs.to_vec(), &self.save_ctxt),
                },
            );
        }

        self.nest_tables(self.tcx.hir().local_def_id(item.hir_id), |v| {
            for field in def.fields() {
                v.process_struct_field_def(field, item.hir_id);
                v.visit_ty(&field.ty);
            }

            v.process_generic_params(ty_params, &qualname, item.hir_id);
        });
    }

    fn process_enum(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        enum_definition: &'tcx hir::EnumDef<'tcx>,
        ty_params: &'tcx hir::Generics<'tcx>,
    ) {
        let enum_data = self.save_ctxt.get_item_data(item);
        let enum_data = match enum_data {
            None => return,
            Some(data) => data,
        };
        down_cast_data!(enum_data, DefData, item.span);

        let access = access_from!(self.save_ctxt, item, item.hir_id);

        for variant in enum_definition.variants {
            let name = variant.ident.name.to_string();
            let qualname = format!("{}::{}", enum_data.qualname, name);
            let name_span = variant.ident.span;

            match variant.data {
                hir::VariantData::Struct(ref fields, ..) => {
                    let fields_str =
                        fields.iter().map(|f| f.ident.to_string()).collect::<Vec<_>>().join(", ");
                    let value = format!("{}::{} {{ {} }}", enum_data.name, name, fields_str);
                    if !self.span.filter_generated(name_span) {
                        let span = self.span_from_span(name_span);
                        let id = id_from_hir_id(variant.id, &self.save_ctxt);
                        let parent = Some(id_from_hir_id(item.hir_id, &self.save_ctxt));

                        self.dumper.dump_def(
                            &access,
                            Def {
                                kind: DefKind::StructVariant,
                                id,
                                span,
                                name,
                                qualname,
                                value,
                                parent,
                                children: vec![],
                                decl_id: None,
                                docs: self.save_ctxt.docs_for_attrs(&variant.attrs),
                                sig: sig::variant_signature(variant, &self.save_ctxt),
                                attributes: lower_attributes(
                                    variant.attrs.to_vec(),
                                    &self.save_ctxt,
                                ),
                            },
                        );
                    }
                }
                ref v => {
                    let mut value = format!("{}::{}", enum_data.name, name);
                    if let &hir::VariantData::Tuple(ref fields, _) = v {
                        value.push('(');
                        value.push_str(
                            &fields
                                .iter()
                                .map(|f| ty_to_string(&f.ty))
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                        value.push(')');
                    }
                    if !self.span.filter_generated(name_span) {
                        let span = self.span_from_span(name_span);
                        let id = id_from_hir_id(variant.id, &self.save_ctxt);
                        let parent = Some(id_from_hir_id(item.hir_id, &self.save_ctxt));

                        self.dumper.dump_def(
                            &access,
                            Def {
                                kind: DefKind::TupleVariant,
                                id,
                                span,
                                name,
                                qualname,
                                value,
                                parent,
                                children: vec![],
                                decl_id: None,
                                docs: self.save_ctxt.docs_for_attrs(&variant.attrs),
                                sig: sig::variant_signature(variant, &self.save_ctxt),
                                attributes: lower_attributes(
                                    variant.attrs.to_vec(),
                                    &self.save_ctxt,
                                ),
                            },
                        );
                    }
                }
            }

            for field in variant.data.fields() {
                self.process_struct_field_def(field, variant.id);
                self.visit_ty(field.ty);
            }
        }
        self.process_generic_params(ty_params, &enum_data.qualname, item.hir_id);
        self.dumper.dump_def(&access, enum_data);
    }

    fn process_impl(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        generics: &'tcx hir::Generics<'tcx>,
        trait_ref: &'tcx Option<hir::TraitRef<'tcx>>,
        typ: &'tcx hir::Ty<'tcx>,
        impl_items: &'tcx [hir::ImplItemRef<'tcx>],
    ) {
        if let Some(impl_data) = self.save_ctxt.get_item_data(item) {
            if !self.span.filter_generated(item.span) {
                if let super::Data::RelationData(rel, imp) = impl_data {
                    self.dumper.dump_relation(rel);
                    self.dumper.dump_impl(imp);
                } else {
                    span_bug!(item.span, "unexpected data kind: {:?}", impl_data);
                }
            }
        }

        let map = &self.tcx.hir();
        self.nest_tables(map.local_def_id(item.hir_id), |v| {
            v.visit_ty(&typ);
            if let &Some(ref trait_ref) = trait_ref {
                v.process_path(trait_ref.hir_ref_id, &trait_ref.path);
            }
            v.process_generic_params(generics, "", item.hir_id);
            for impl_item in impl_items {
                v.process_impl_item(
                    map.impl_item(impl_item.id),
                    map.local_def_id(item.hir_id).to_def_id(),
                );
            }
        });
    }

    fn process_trait(
        &mut self,
        item: &'tcx hir::Item<'tcx>,
        generics: &'tcx hir::Generics<'tcx>,
        trait_refs: hir::GenericBounds<'tcx>,
        methods: &'tcx [hir::TraitItemRef],
    ) {
        let name = item.ident.to_string();
        let qualname = format!(
            "::{}",
            self.tcx.def_path_str(self.tcx.hir().local_def_id(item.hir_id).to_def_id())
        );
        let mut val = name.clone();
        if !generics.params.is_empty() {
            val.push_str(&generic_params_to_string(generics.params));
        }
        if !trait_refs.is_empty() {
            val.push_str(": ");
            val.push_str(&bounds_to_string(trait_refs));
        }
        if !self.span.filter_generated(item.ident.span) {
            let id = id_from_hir_id(item.hir_id, &self.save_ctxt);
            let span = self.span_from_span(item.ident.span);
            let children =
                methods.iter().map(|i| id_from_hir_id(i.id.hir_id, &self.save_ctxt)).collect();
            self.dumper.dump_def(
                &access_from!(self.save_ctxt, item, item.hir_id),
                Def {
                    kind: DefKind::Trait,
                    id,
                    span,
                    name,
                    qualname: qualname.clone(),
                    value: val,
                    parent: None,
                    children,
                    decl_id: None,
                    docs: self.save_ctxt.docs_for_attrs(&item.attrs),
                    sig: sig::item_signature(item, &self.save_ctxt),
                    attributes: lower_attributes(item.attrs.to_vec(), &self.save_ctxt),
                },
            );
        }

        // super-traits
        for super_bound in trait_refs.iter() {
            let trait_ref = match *super_bound {
                hir::GenericBound::Trait(ref trait_ref, _) => trait_ref,
                hir::GenericBound::Outlives(..) => continue,
            };

            let trait_ref = &trait_ref.trait_ref;
            if let Some(id) = self.lookup_def_id(trait_ref.hir_ref_id) {
                let sub_span = trait_ref.path.segments.last().unwrap().ident.span;
                if !self.span.filter_generated(sub_span) {
                    let span = self.span_from_span(sub_span);
                    self.dumper.dump_ref(Ref {
                        kind: RefKind::Type,
                        span: span.clone(),
                        ref_id: id_from_def_id(id),
                    });

                    self.dumper.dump_relation(Relation {
                        kind: RelationKind::SuperTrait,
                        span,
                        from: id_from_def_id(id),
                        to: id_from_hir_id(item.hir_id, &self.save_ctxt),
                    });
                }
            }
        }

        // walk generics and methods
        self.process_generic_params(generics, &qualname, item.hir_id);
        for method in methods {
            let map = &self.tcx.hir();
            self.process_trait_item(
                map.trait_item(method.id),
                map.local_def_id(item.hir_id).to_def_id(),
            )
        }
    }

    // `item` is the module in question, represented as an( item.
    fn process_mod(&mut self, item: &'tcx hir::Item<'tcx>) {
        if let Some(mod_data) = self.save_ctxt.get_item_data(item) {
            down_cast_data!(mod_data, DefData, item.span);
            self.dumper.dump_def(&access_from!(self.save_ctxt, item, item.hir_id), mod_data);
        }
    }

    fn dump_path_ref(&mut self, id: hir::HirId, path: &hir::Path<'tcx>) {
        let path_data = self.save_ctxt.get_path_data(id, path);
        if let Some(path_data) = path_data {
            self.dumper.dump_ref(path_data);
        }
    }

    fn dump_path_segment_ref(&mut self, id: hir::HirId, segment: &hir::PathSegment<'tcx>) {
        let segment_data = self.save_ctxt.get_path_segment_data_with_id(segment, id);
        if let Some(segment_data) = segment_data {
            self.dumper.dump_ref(segment_data);
        }
    }

    fn process_path(&mut self, id: hir::HirId, path: &'tcx hir::Path<'tcx>) {
        if self.span.filter_generated(path.span) {
            return;
        }
        self.dump_path_ref(id, path);

        // Type arguments
        for seg in path.segments {
            if let Some(ref generic_args) = seg.args {
                for arg in generic_args.args {
                    if let hir::GenericArg::Type(ref ty) = arg {
                        self.visit_ty(ty);
                    }
                }
            }
        }

        self.write_sub_paths_truncated(path);
    }

    fn process_struct_lit(
        &mut self,
        ex: &'tcx hir::Expr<'tcx>,
        path: &'tcx hir::QPath<'tcx>,
        fields: &'tcx [hir::Field<'tcx>],
        variant: &'tcx ty::VariantDef,
        base: Option<&'tcx hir::Expr<'tcx>>,
    ) {
        if let Some(struct_lit_data) = self.save_ctxt.get_expr_data(ex) {
            if let hir::QPath::Resolved(_, path) = path {
                self.write_sub_paths_truncated(path);
            }
            down_cast_data!(struct_lit_data, RefData, ex.span);
            if !generated_code(ex.span) {
                self.dumper.dump_ref(struct_lit_data);
            }

            for field in fields {
                if let Some(field_data) = self.save_ctxt.get_field_ref_data(field, variant) {
                    self.dumper.dump_ref(field_data);
                }

                self.visit_expr(&field.expr)
            }
        }

        walk_list!(self, visit_expr, base);
    }

    fn process_method_call(
        &mut self,
        ex: &'tcx hir::Expr<'tcx>,
        seg: &'tcx hir::PathSegment<'tcx>,
        args: &'tcx [hir::Expr<'tcx>],
    ) {
        debug!("process_method_call {:?} {:?}", ex, ex.span);
        if let Some(mcd) = self.save_ctxt.get_expr_data(ex) {
            down_cast_data!(mcd, RefData, ex.span);
            if !generated_code(ex.span) {
                self.dumper.dump_ref(mcd);
            }
        }

        // Explicit types in the turbo-fish.
        if let Some(generic_args) = seg.args {
            for arg in generic_args.args {
                if let hir::GenericArg::Type(ty) = arg {
                    self.visit_ty(&ty)
                };
            }
        }

        // walk receiver and args
        walk_list!(self, visit_expr, args);
    }

    fn process_pat(&mut self, p: &'tcx hir::Pat<'tcx>) {
        match p.kind {
            hir::PatKind::Struct(ref _path, fields, _) => {
                // FIXME do something with _path?
                let adt = match self.save_ctxt.tables.node_type_opt(p.hir_id) {
                    Some(ty) if ty.ty_adt_def().is_some() => ty.ty_adt_def().unwrap(),
                    _ => {
                        intravisit::walk_pat(self, p);
                        return;
                    }
                };
                let variant = adt.variant_of_res(self.save_ctxt.get_path_res(p.hir_id));

                for field in fields {
                    if let Some(index) = self.tcx.find_field_index(field.ident, variant) {
                        if !self.span.filter_generated(field.ident.span) {
                            let span = self.span_from_span(field.ident.span);
                            self.dumper.dump_ref(Ref {
                                kind: RefKind::Variable,
                                span,
                                ref_id: id_from_def_id(variant.fields[index].did),
                            });
                        }
                    }
                    self.visit_pat(&field.pat);
                }
            }
            _ => intravisit::walk_pat(self, p),
        }
    }

    fn process_var_decl(&mut self, pat: &'tcx hir::Pat<'tcx>) {
        // The pattern could declare multiple new vars,
        // we must walk the pattern and collect them all.
        let mut collector = PathCollector::new(self.tcx);
        collector.visit_pat(&pat);
        self.visit_pat(&pat);

        // Process collected paths.
        for (id, ident, _) in collector.collected_idents {
            let res = self.save_ctxt.get_path_res(id);
            match res {
                Res::Local(hir_id) => {
                    let typ = self
                        .save_ctxt
                        .tables
                        .node_type_opt(hir_id)
                        .map(|t| t.to_string())
                        .unwrap_or_default();

                    // Rust uses the id of the pattern for var lookups, so we'll use it too.
                    if !self.span.filter_generated(ident.span) {
                        let qualname = format!("{}${}", ident.to_string(), hir_id);
                        let id = id_from_hir_id(hir_id, &self.save_ctxt);
                        let span = self.span_from_span(ident.span);

                        self.dumper.dump_def(
                            &Access { public: false, reachable: false },
                            Def {
                                kind: DefKind::Local,
                                id,
                                span,
                                name: ident.to_string(),
                                qualname,
                                value: typ,
                                parent: None,
                                children: vec![],
                                decl_id: None,
                                docs: String::new(),
                                sig: None,
                                attributes: vec![],
                            },
                        );
                    }
                }
                Res::Def(
                    HirDefKind::Ctor(..)
                    | HirDefKind::Const
                    | HirDefKind::AssocConst
                    | HirDefKind::Struct
                    | HirDefKind::Variant
                    | HirDefKind::TyAlias
                    | HirDefKind::AssocTy,
                    _,
                )
                | Res::SelfTy(..) => {
                    self.dump_path_segment_ref(id, &hir::PathSegment::from_ident(ident));
                }
                def => {
                    error!("unexpected definition kind when processing collected idents: {:?}", def)
                }
            }
        }

        for (id, ref path) in collector.collected_paths {
            if let hir::QPath::Resolved(_, path) = path {
                self.process_path(id, path);
            }
        }
    }

    /// Extracts macro use and definition information from the AST node defined
    /// by the given NodeId, using the expansion information from the node's
    /// span.
    ///
    /// If the span is not macro-generated, do nothing, else use callee and
    /// callsite spans to record macro definition and use data, using the
    /// mac_uses and mac_defs sets to prevent multiples.
    fn process_macro_use(&mut self, _span: Span) {
        // FIXME if we're not dumping the defs (see below), there is no point
        // dumping refs either.
        // let source_span = span.source_callsite();
        // if !self.macro_calls.insert(source_span) {
        //     return;
        // }

        // let data = match self.save_ctxt.get_macro_use_data(span) {
        //     None => return,
        //     Some(data) => data,
        // };

        // self.dumper.macro_use(data);

        // FIXME write the macro def
        // let mut hasher = DefaultHasher::new();
        // data.callee_span.hash(&mut hasher);
        // let hash = hasher.finish();
        // let qualname = format!("{}::{}", data.name, hash);
        // Don't write macro definition for imported macros
        // if !self.mac_defs.contains(&data.callee_span)
        //     && !data.imported {
        //     self.mac_defs.insert(data.callee_span);
        //     if let Some(sub_span) = self.span.span_for_macro_def_name(data.callee_span) {
        //         self.dumper.macro_data(MacroData {
        //             span: sub_span,
        //             name: data.name.clone(),
        //             qualname: qualname.clone(),
        //             // FIXME where do macro docs come from?
        //             docs: String::new(),
        //         }.lower(self.tcx));
        //     }
        // }
    }

    fn process_trait_item(&mut self, trait_item: &'tcx hir::TraitItem<'tcx>, trait_id: DefId) {
        self.process_macro_use(trait_item.span);
        let vis_span = trait_item.span.shrink_to_lo();
        match trait_item.kind {
            hir::TraitItemKind::Const(ref ty, body) => {
                let body = body.map(|b| &self.tcx.hir().body(b).value);
                let respan = respan(vis_span, hir::VisibilityKind::Public);
                self.process_assoc_const(
                    trait_item.hir_id,
                    trait_item.ident,
                    &ty,
                    body,
                    trait_id,
                    &respan,
                    &trait_item.attrs,
                );
            }
            hir::TraitItemKind::Fn(ref sig, ref trait_fn) => {
                let body =
                    if let hir::TraitFn::Provided(body) = trait_fn { Some(*body) } else { None };
                let respan = respan(vis_span, hir::VisibilityKind::Public);
                self.process_method(
                    sig,
                    body,
                    trait_item.hir_id,
                    trait_item.ident,
                    &trait_item.generics,
                    &respan,
                    trait_item.span,
                );
            }
            hir::TraitItemKind::Type(ref bounds, ref default_ty) => {
                // FIXME do something with _bounds (for type refs)
                let name = trait_item.ident.name.to_string();
                let qualname = format!(
                    "::{}",
                    self.tcx
                        .def_path_str(self.tcx.hir().local_def_id(trait_item.hir_id).to_def_id())
                );

                if !self.span.filter_generated(trait_item.ident.span) {
                    let span = self.span_from_span(trait_item.ident.span);
                    let id = id_from_hir_id(trait_item.hir_id, &self.save_ctxt);

                    self.dumper.dump_def(
                        &Access { public: true, reachable: true },
                        Def {
                            kind: DefKind::Type,
                            id,
                            span,
                            name,
                            qualname,
                            value: self.span.snippet(trait_item.span),
                            parent: Some(id_from_def_id(trait_id)),
                            children: vec![],
                            decl_id: None,
                            docs: self.save_ctxt.docs_for_attrs(&trait_item.attrs),
                            sig: sig::assoc_type_signature(
                                trait_item.hir_id,
                                trait_item.ident,
                                Some(bounds),
                                default_ty.as_ref().map(|ty| &**ty),
                                &self.save_ctxt,
                            ),
                            attributes: lower_attributes(
                                trait_item.attrs.to_vec(),
                                &self.save_ctxt,
                            ),
                        },
                    );
                }

                if let &Some(ref default_ty) = default_ty {
                    self.visit_ty(default_ty)
                }
            }
        }
    }

    fn process_impl_item(&mut self, impl_item: &'tcx hir::ImplItem<'tcx>, impl_id: DefId) {
        self.process_macro_use(impl_item.span);
        match impl_item.kind {
            hir::ImplItemKind::Const(ref ty, body) => {
                let body = self.tcx.hir().body(body);
                self.process_assoc_const(
                    impl_item.hir_id,
                    impl_item.ident,
                    &ty,
                    Some(&body.value),
                    impl_id,
                    &impl_item.vis,
                    &impl_item.attrs,
                );
            }
            hir::ImplItemKind::Fn(ref sig, body) => {
                self.process_method(
                    sig,
                    Some(body),
                    impl_item.hir_id,
                    impl_item.ident,
                    &impl_item.generics,
                    &impl_item.vis,
                    impl_item.span,
                );
            }
            hir::ImplItemKind::OpaqueTy(..) => {}
            hir::ImplItemKind::TyAlias(ref ty) => {
                // FIXME: uses of the assoc type should ideally point to this
                // 'def' and the name here should be a ref to the def in the
                // trait.
                self.visit_ty(ty)
            }
        }
    }

    pub(crate) fn process_crate(&mut self, krate: &'tcx hir::Crate<'tcx>) {
        let id = hir::CRATE_HIR_ID;
        let qualname =
            format!("::{}", self.tcx.def_path_str(self.tcx.hir().local_def_id(id).to_def_id()));

        let sm = self.tcx.sess.source_map();
        let filename = sm.span_to_filename(krate.item.span);
        let data_id = id_from_hir_id(id, &self.save_ctxt);
        let children = krate
            .item
            .module
            .item_ids
            .iter()
            .map(|i| id_from_hir_id(i.id, &self.save_ctxt))
            .collect();
        let span = self.span_from_span(krate.item.span);

        self.dumper.dump_def(
            &Access { public: true, reachable: true },
            Def {
                kind: DefKind::Mod,
                id: data_id,
                name: String::new(),
                qualname,
                span,
                value: filename.to_string(),
                children,
                parent: None,
                decl_id: None,
                docs: self.save_ctxt.docs_for_attrs(krate.item.attrs),
                sig: None,
                attributes: lower_attributes(krate.item.attrs.to_owned(), &self.save_ctxt),
            },
        );
        intravisit::walk_crate(self, krate);
    }

    fn process_bounds(&mut self, bounds: hir::GenericBounds<'tcx>) {
        for bound in bounds {
            if let hir::GenericBound::Trait(ref trait_ref, _) = *bound {
                self.process_path(trait_ref.trait_ref.hir_ref_id, &trait_ref.trait_ref.path)
            }
        }
    }
}

impl<'l, 'tcx> Visitor<'tcx> for DumpVisitor<'l, 'tcx> {
    type Map = Map<'tcx>;

    fn nested_visit_map(&mut self) -> intravisit::NestedVisitorMap<Self::Map> {
        intravisit::NestedVisitorMap::All(self.tcx.hir())
    }

    fn visit_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        self.process_macro_use(item.span);
        match item.kind {
            hir::ItemKind::Use(path, hir::UseKind::Single) => {
                let sub_span = path.segments.last().unwrap().ident.span;
                if !self.span.filter_generated(sub_span) {
                    let access = access_from!(self.save_ctxt, item, item.hir_id);
                    let ref_id = self.lookup_def_id(item.hir_id).map(id_from_def_id);
                    let span = self.span_from_span(sub_span);
                    let parent = self
                        .save_ctxt
                        .tcx
                        .hir()
                        .opt_local_def_id(item.hir_id)
                        .and_then(|id| self.save_ctxt.tcx.parent(id.to_def_id()))
                        .map(id_from_def_id);
                    self.dumper.import(
                        &access,
                        Import {
                            kind: ImportKind::Use,
                            ref_id,
                            span,
                            alias_span: None,
                            name: item.ident.to_string(),
                            value: String::new(),
                            parent,
                        },
                    );
                    self.write_sub_paths_truncated(&path);
                }
            }
            hir::ItemKind::Use(path, hir::UseKind::Glob) => {
                // Make a comma-separated list of names of imported modules.
                let def_id = self.tcx.hir().local_def_id(item.hir_id);
                let names = self.tcx.names_imported_by_glob_use(def_id);
                let names: Vec<_> = names.iter().map(|n| n.to_string()).collect();

                // Otherwise it's a span with wrong macro expansion info, which
                // we don't want to track anyway, since it's probably macro-internal `use`
                if let Some(sub_span) =
                    self.span.sub_span_of_token(item.span, token::BinOp(token::Star))
                {
                    if !self.span.filter_generated(item.span) {
                        let access = access_from!(self.save_ctxt, item, item.hir_id);
                        let span = self.span_from_span(sub_span);
                        let parent = self
                            .save_ctxt
                            .tcx
                            .hir()
                            .opt_local_def_id(item.hir_id)
                            .and_then(|id| self.save_ctxt.tcx.parent(id.to_def_id()))
                            .map(id_from_def_id);
                        self.dumper.import(
                            &access,
                            Import {
                                kind: ImportKind::GlobUse,
                                ref_id: None,
                                span,
                                alias_span: None,
                                name: "*".to_owned(),
                                value: names.join(", "),
                                parent,
                            },
                        );
                        self.write_sub_paths(&path);
                    }
                }
            }
            hir::ItemKind::ExternCrate(_) => {
                let name_span = item.ident.span;
                if !self.span.filter_generated(name_span) {
                    let span = self.span_from_span(name_span);
                    let parent = self
                        .save_ctxt
                        .tcx
                        .hir()
                        .opt_local_def_id(item.hir_id)
                        .and_then(|id| self.save_ctxt.tcx.parent(id.to_def_id()))
                        .map(id_from_def_id);
                    self.dumper.import(
                        &Access { public: false, reachable: false },
                        Import {
                            kind: ImportKind::ExternCrate,
                            ref_id: None,
                            span,
                            alias_span: None,
                            name: item.ident.to_string(),
                            value: String::new(),
                            parent,
                        },
                    );
                }
            }
            hir::ItemKind::Fn(ref sig, ref ty_params, body) => {
                self.process_fn(item, sig.decl, &sig.header, ty_params, body)
            }
            hir::ItemKind::Static(ref typ, _, body) => {
                let body = self.tcx.hir().body(body);
                self.process_static_or_const_item(item, typ, &body.value)
            }
            hir::ItemKind::Const(ref typ, body) => {
                let body = self.tcx.hir().body(body);
                self.process_static_or_const_item(item, typ, &body.value)
            }
            hir::ItemKind::Struct(ref def, ref ty_params)
            | hir::ItemKind::Union(ref def, ref ty_params) => {
                self.process_struct(item, def, ty_params)
            }
            hir::ItemKind::Enum(ref def, ref ty_params) => self.process_enum(item, def, ty_params),
            hir::ItemKind::Impl { ref generics, ref of_trait, ref self_ty, ref items, .. } => {
                self.process_impl(item, generics, of_trait, &self_ty, items)
            }
            hir::ItemKind::Trait(_, _, ref generics, ref trait_refs, methods) => {
                self.process_trait(item, generics, trait_refs, methods)
            }
            hir::ItemKind::Mod(ref m) => {
                self.process_mod(item);
                intravisit::walk_mod(self, m, item.hir_id);
            }
            hir::ItemKind::TyAlias(ty, ref generics) => {
                let qualname = format!(
                    "::{}",
                    self.tcx.def_path_str(self.tcx.hir().local_def_id(item.hir_id).to_def_id())
                );
                let value = ty_to_string(&ty);
                if !self.span.filter_generated(item.ident.span) {
                    let span = self.span_from_span(item.ident.span);
                    let id = id_from_hir_id(item.hir_id, &self.save_ctxt);

                    self.dumper.dump_def(
                        &access_from!(self.save_ctxt, item, item.hir_id),
                        Def {
                            kind: DefKind::Type,
                            id,
                            span,
                            name: item.ident.to_string(),
                            qualname: qualname.clone(),
                            value,
                            parent: None,
                            children: vec![],
                            decl_id: None,
                            docs: self.save_ctxt.docs_for_attrs(&item.attrs),
                            sig: sig::item_signature(item, &self.save_ctxt),
                            attributes: lower_attributes(item.attrs.to_vec(), &self.save_ctxt),
                        },
                    );
                }

                self.visit_ty(ty);
                self.process_generic_params(generics, &qualname, item.hir_id);
            }
            _ => intravisit::walk_item(self, item),
        }
    }

    fn visit_generics(&mut self, generics: &'tcx hir::Generics<'tcx>) {
        for param in generics.params {
            match param.kind {
                hir::GenericParamKind::Lifetime { .. } => {}
                hir::GenericParamKind::Type { ref default, .. } => {
                    self.process_bounds(param.bounds);
                    if let Some(ref ty) = default {
                        self.visit_ty(ty);
                    }
                }
                hir::GenericParamKind::Const { ref ty } => {
                    self.process_bounds(param.bounds);
                    self.visit_ty(ty);
                }
            }
        }
        for pred in generics.where_clause.predicates {
            if let hir::WherePredicate::BoundPredicate(ref wbp) = *pred {
                self.process_bounds(wbp.bounds);
                self.visit_ty(wbp.bounded_ty);
            }
        }
    }

    fn visit_ty(&mut self, t: &'tcx hir::Ty<'tcx>) {
        self.process_macro_use(t.span);
        match t.kind {
            hir::TyKind::Path(hir::QPath::Resolved(_, path)) => {
                if generated_code(t.span) {
                    return;
                }

                if let Some(id) = self.lookup_def_id(t.hir_id) {
                    let sub_span = path.segments.last().unwrap().ident.span;
                    let span = self.span_from_span(sub_span);
                    self.dumper.dump_ref(Ref {
                        kind: RefKind::Type,
                        span,
                        ref_id: id_from_def_id(id),
                    });
                }

                self.write_sub_paths_truncated(path);
                intravisit::walk_path(self, path);
            }
            hir::TyKind::Array(ref ty, ref anon_const) => {
                self.visit_ty(ty);
                let map = self.tcx.hir();
                self.nest_tables(self.tcx.hir().local_def_id(anon_const.hir_id), |v| {
                    v.visit_expr(&map.body(anon_const.body).value)
                });
            }
            _ => intravisit::walk_ty(self, t),
        }
    }

    fn visit_expr(&mut self, ex: &'tcx hir::Expr<'tcx>) {
        debug!("visit_expr {:?}", ex.kind);
        self.process_macro_use(ex.span);
        match ex.kind {
            hir::ExprKind::Struct(ref path, ref fields, ref base) => {
                let hir_expr = self.save_ctxt.tcx.hir().expect_expr(ex.hir_id);
                let adt = match self.save_ctxt.tables.expr_ty_opt(&hir_expr) {
                    Some(ty) if ty.ty_adt_def().is_some() => ty.ty_adt_def().unwrap(),
                    _ => {
                        intravisit::walk_expr(self, ex);
                        return;
                    }
                };
                let res = self.save_ctxt.get_path_res(hir_expr.hir_id);
                self.process_struct_lit(ex, path, fields, adt.variant_of_res(res), *base)
            }
            hir::ExprKind::MethodCall(ref seg, _, args, _) => {
                self.process_method_call(ex, seg, args)
            }
            hir::ExprKind::Field(ref sub_ex, _) => {
                self.visit_expr(&sub_ex);

                if let Some(field_data) = self.save_ctxt.get_expr_data(ex) {
                    down_cast_data!(field_data, RefData, ex.span);
                    if !generated_code(ex.span) {
                        self.dumper.dump_ref(field_data);
                    }
                }
            }
            hir::ExprKind::Closure(_, ref decl, body, _fn_decl_span, _) => {
                let id = format!("${}", ex.hir_id);

                // walk arg and return types
                for ty in decl.inputs {
                    self.visit_ty(ty);
                }

                if let hir::FnRetTy::Return(ref ret_ty) = decl.output {
                    self.visit_ty(ret_ty);
                }

                // walk the body
                let map = self.tcx.hir();
                self.nest_tables(self.tcx.hir().local_def_id(ex.hir_id), |v| {
                    let body = map.body(body);
                    v.process_formals(body.params, &id);
                    v.visit_expr(&body.value)
                });
            }
            hir::ExprKind::Repeat(ref expr, ref anon_const) => {
                self.visit_expr(expr);
                let map = self.tcx.hir();
                self.nest_tables(self.tcx.hir().local_def_id(anon_const.hir_id), |v| {
                    v.visit_expr(&map.body(anon_const.body).value)
                });
            }
            // In particular, we take this branch for call and path expressions,
            // where we'll index the idents involved just by continuing to walk.
            _ => intravisit::walk_expr(self, ex),
        }
    }

    fn visit_pat(&mut self, p: &'tcx hir::Pat<'tcx>) {
        self.process_macro_use(p.span);
        self.process_pat(p);
    }

    fn visit_arm(&mut self, arm: &'tcx hir::Arm<'tcx>) {
        self.process_var_decl(&arm.pat);
        if let Some(hir::Guard::If(expr)) = &arm.guard {
            self.visit_expr(expr);
        }
        self.visit_expr(&arm.body);
    }

    fn visit_path(&mut self, p: &'tcx hir::Path<'tcx>, id: hir::HirId) {
        self.process_path(id, p);
    }

    fn visit_stmt(&mut self, s: &'tcx hir::Stmt<'tcx>) {
        self.process_macro_use(s.span);
        intravisit::walk_stmt(self, s)
    }

    fn visit_local(&mut self, l: &'tcx hir::Local<'tcx>) {
        self.process_macro_use(l.span);
        self.process_var_decl(&l.pat);

        // Just walk the initialiser and type (don't want to walk the pattern again).
        walk_list!(self, visit_ty, &l.ty);
        walk_list!(self, visit_expr, &l.init);
    }

    fn visit_foreign_item(&mut self, item: &'tcx hir::ForeignItem<'tcx>) {
        let access = access_from!(self.save_ctxt, item, item.hir_id);

        match item.kind {
            hir::ForeignItemKind::Fn(decl, _, ref generics) => {
                if let Some(fn_data) = self.save_ctxt.get_extern_item_data(item) {
                    down_cast_data!(fn_data, DefData, item.span);

                    self.process_generic_params(generics, &fn_data.qualname, item.hir_id);
                    self.dumper.dump_def(&access, fn_data);
                }

                for ty in decl.inputs {
                    self.visit_ty(ty);
                }

                if let hir::FnRetTy::Return(ref ret_ty) = decl.output {
                    self.visit_ty(ret_ty);
                }
            }
            hir::ForeignItemKind::Static(ref ty, _) => {
                if let Some(var_data) = self.save_ctxt.get_extern_item_data(item) {
                    down_cast_data!(var_data, DefData, item.span);
                    self.dumper.dump_def(&access, var_data);
                }

                self.visit_ty(ty);
            }
            hir::ForeignItemKind::Type => {
                if let Some(var_data) = self.save_ctxt.get_extern_item_data(item) {
                    down_cast_data!(var_data, DefData, item.span);
                    self.dumper.dump_def(&access, var_data);
                }
            }
        }
    }
}
