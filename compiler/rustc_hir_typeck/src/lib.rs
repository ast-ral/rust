#![feature(if_let_guard)]
#![feature(let_chains)]
#![feature(try_blocks)]
#![feature(never_type)]
#![feature(min_specialization)]
#![feature(control_flow_enum)]
#![feature(drain_filter)]
#![allow(rustc::potential_query_instability)]
#![recursion_limit = "256"]

#[macro_use]
extern crate tracing;

#[macro_use]
extern crate rustc_middle;

mod _match;
mod autoderef;
mod callee;
// Used by clippy;
pub mod cast;
mod check;
mod closure;
mod coercion;
mod demand;
mod diverges;
mod errors;
mod expectation;
mod expr;
// Used by clippy;
pub mod expr_use_visitor;
mod fallback;
mod fn_ctxt;
mod gather_locals;
mod generator_interior;
mod inherited;
mod intrinsicck;
mod mem_categorization;
mod method;
mod op;
mod pat;
mod place_op;
mod rvalue_scopes;
mod upvar;
mod writeback;

pub use diverges::Diverges;
pub use expectation::Expectation;
pub use fn_ctxt::*;
pub use inherited::{Inherited, InheritedBuilder};

use crate::check::check_fn;
use crate::coercion::DynamicCoerceMany;
use crate::gather_locals::GatherLocalsVisitor;
use rustc_data_structures::unord::UnordSet;
use rustc_errors::{struct_span_err, DiagnosticId, ErrorGuaranteed, MultiSpan};
use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::intravisit::Visitor;
use rustc_hir::{HirIdMap, Node};
use rustc_hir_analysis::astconv::AstConv;
use rustc_hir_analysis::check::check_abi;
use rustc_infer::infer::type_variable::{TypeVariableOrigin, TypeVariableOriginKind};
use rustc_middle::traits;
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::{self, Ty, TyCtxt};
use rustc_session::config;
use rustc_session::Session;
use rustc_span::def_id::{DefId, LocalDefId};
use rustc_span::Span;

#[macro_export]
macro_rules! type_error_struct {
    ($session:expr, $span:expr, $typ:expr, $code:ident, $($message:tt)*) => ({
        let mut err = rustc_errors::struct_span_err!($session, $span, $code, $($message)*);

        if $typ.references_error() {
            err.downgrade_to_delayed_bug();
        }

        err
    })
}

/// The type of a local binding, including the revealed type for anon types.
#[derive(Copy, Clone, Debug)]
pub struct LocalTy<'tcx> {
    decl_ty: Ty<'tcx>,
    revealed_ty: Ty<'tcx>,
}

/// If this `DefId` is a "primary tables entry", returns
/// `Some((body_id, body_ty, fn_sig))`. Otherwise, returns `None`.
///
/// If this function returns `Some`, then `typeck_results(def_id)` will
/// succeed; if it returns `None`, then `typeck_results(def_id)` may or
/// may not succeed. In some cases where this function returns `None`
/// (notably closures), `typeck_results(def_id)` would wind up
/// redirecting to the owning function.
fn primary_body_of(
    tcx: TyCtxt<'_>,
    id: hir::HirId,
) -> Option<(hir::BodyId, Option<&hir::Ty<'_>>, Option<&hir::FnSig<'_>>)> {
    match tcx.hir().get(id) {
        Node::Item(item) => match item.kind {
            hir::ItemKind::Const(ty, body) | hir::ItemKind::Static(ty, _, body) => {
                Some((body, Some(ty), None))
            }
            hir::ItemKind::Fn(ref sig, .., body) => Some((body, None, Some(sig))),
            _ => None,
        },
        Node::TraitItem(item) => match item.kind {
            hir::TraitItemKind::Const(ty, Some(body)) => Some((body, Some(ty), None)),
            hir::TraitItemKind::Fn(ref sig, hir::TraitFn::Provided(body)) => {
                Some((body, None, Some(sig)))
            }
            _ => None,
        },
        Node::ImplItem(item) => match item.kind {
            hir::ImplItemKind::Const(ty, body) => Some((body, Some(ty), None)),
            hir::ImplItemKind::Fn(ref sig, body) => Some((body, None, Some(sig))),
            _ => None,
        },
        Node::AnonConst(constant) => Some((constant.body, None, None)),
        _ => None,
    }
}

fn has_typeck_results(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    // Closures' typeck results come from their outermost function,
    // as they are part of the same "inference environment".
    let typeck_root_def_id = tcx.typeck_root_def_id(def_id);
    if typeck_root_def_id != def_id {
        return tcx.has_typeck_results(typeck_root_def_id);
    }

    if let Some(def_id) = def_id.as_local() {
        let id = tcx.hir().local_def_id_to_hir_id(def_id);
        primary_body_of(tcx, id).is_some()
    } else {
        false
    }
}

fn used_trait_imports(tcx: TyCtxt<'_>, def_id: LocalDefId) -> &UnordSet<LocalDefId> {
    &*tcx.typeck(def_id).used_trait_imports
}

fn typeck_item_bodies(tcx: TyCtxt<'_>, (): ()) {
    tcx.hir().par_body_owners(|body_owner_def_id| tcx.ensure().typeck(body_owner_def_id));
}

fn typeck_const_arg<'tcx>(
    tcx: TyCtxt<'tcx>,
    (did, param_did): (LocalDefId, DefId),
) -> &ty::TypeckResults<'tcx> {
    let fallback = move || tcx.type_of(param_did);
    typeck_with_fallback(tcx, did, fallback)
}

fn typeck<'tcx>(tcx: TyCtxt<'tcx>, def_id: LocalDefId) -> &ty::TypeckResults<'tcx> {
    if let Some(param_did) = tcx.opt_const_param_of(def_id) {
        tcx.typeck_const_arg((def_id, param_did))
    } else {
        let fallback = move || tcx.type_of(def_id.to_def_id());
        typeck_with_fallback(tcx, def_id, fallback)
    }
}

/// Used only to get `TypeckResults` for type inference during error recovery.
/// Currently only used for type inference of `static`s and `const`s to avoid type cycle errors.
fn diagnostic_only_typeck<'tcx>(tcx: TyCtxt<'tcx>, def_id: LocalDefId) -> &ty::TypeckResults<'tcx> {
    let fallback = move || {
        let span = tcx.hir().span(tcx.hir().local_def_id_to_hir_id(def_id));
        tcx.ty_error_with_message(span, "diagnostic only typeck table used")
    };
    typeck_with_fallback(tcx, def_id, fallback)
}

#[instrument(level = "debug", skip(tcx, fallback), ret)]
fn typeck_with_fallback<'tcx>(
    tcx: TyCtxt<'tcx>,
    def_id: LocalDefId,
    fallback: impl Fn() -> Ty<'tcx> + 'tcx,
) -> &'tcx ty::TypeckResults<'tcx> {
    // Closures' typeck results come from their outermost function,
    // as they are part of the same "inference environment".
    let typeck_root_def_id = tcx.typeck_root_def_id(def_id.to_def_id()).expect_local();
    if typeck_root_def_id != def_id {
        return tcx.typeck(typeck_root_def_id);
    }

    let id = tcx.hir().local_def_id_to_hir_id(def_id);
    let span = tcx.hir().span(id);

    // Figure out what primary body this item has.
    let (body_id, body_ty, fn_sig) = primary_body_of(tcx, id).unwrap_or_else(|| {
        span_bug!(span, "can't type-check body of {:?}", def_id);
    });
    let body = tcx.hir().body(body_id);

    let typeck_results = Inherited::build(tcx, def_id).enter(|inh| {
        let param_env = tcx.param_env(def_id);
        let mut fcx = FnCtxt::new(&inh, param_env, body.value.hir_id);

        if let Some(hir::FnSig { header, decl, .. }) = fn_sig {
            let fn_sig = if rustc_hir_analysis::collect::get_infer_ret_ty(&decl.output).is_some() {
                <dyn AstConv<'_>>::ty_of_fn(&fcx, id, header.unsafety, header.abi, decl, None, None)
            } else {
                tcx.fn_sig(def_id)
            };

            check_abi(tcx, id, span, fn_sig.abi());

            // Compute the function signature from point of view of inside the fn.
            let fn_sig = tcx.liberate_late_bound_regions(def_id.to_def_id(), fn_sig);
            let fn_sig = fcx.normalize(body.value.span, fn_sig);

            check_fn(&mut fcx, fn_sig, decl, def_id, body, None);
        } else {
            let expected_type = body_ty
                .and_then(|ty| match ty.kind {
                    hir::TyKind::Infer => Some(<dyn AstConv<'_>>::ast_ty_to_ty(&fcx, ty)),
                    _ => None,
                })
                .unwrap_or_else(|| match tcx.hir().get(id) {
                    Node::AnonConst(_) => match tcx.hir().get(tcx.hir().parent_id(id)) {
                        Node::Expr(&hir::Expr {
                            kind: hir::ExprKind::ConstBlock(ref anon_const),
                            ..
                        }) if anon_const.hir_id == id => fcx.next_ty_var(TypeVariableOrigin {
                            kind: TypeVariableOriginKind::TypeInference,
                            span,
                        }),
                        Node::Ty(&hir::Ty {
                            kind: hir::TyKind::Typeof(ref anon_const), ..
                        }) if anon_const.hir_id == id => fcx.next_ty_var(TypeVariableOrigin {
                            kind: TypeVariableOriginKind::TypeInference,
                            span,
                        }),
                        Node::Expr(&hir::Expr { kind: hir::ExprKind::InlineAsm(asm), .. })
                        | Node::Item(&hir::Item { kind: hir::ItemKind::GlobalAsm(asm), .. }) => {
                            let operand_ty = asm
                                .operands
                                .iter()
                                .filter_map(|(op, _op_sp)| match op {
                                    hir::InlineAsmOperand::Const { anon_const }
                                        if anon_const.hir_id == id =>
                                    {
                                        // Inline assembly constants must be integers.
                                        Some(fcx.next_int_var())
                                    }
                                    hir::InlineAsmOperand::SymFn { anon_const }
                                        if anon_const.hir_id == id =>
                                    {
                                        Some(fcx.next_ty_var(TypeVariableOrigin {
                                            kind: TypeVariableOriginKind::MiscVariable,
                                            span,
                                        }))
                                    }
                                    _ => None,
                                })
                                .next();
                            operand_ty.unwrap_or_else(fallback)
                        }
                        _ => fallback(),
                    },
                    _ => fallback(),
                });

            let expected_type = fcx.normalize(body.value.span, expected_type);
            fcx.require_type_is_sized(expected_type, body.value.span, traits::ConstSized);

            // Gather locals in statics (because of block expressions).
            GatherLocalsVisitor::new(&fcx).visit_body(body);

            fcx.check_expr_coercable_to_type(&body.value, expected_type, None);

            fcx.write_ty(id, expected_type);
        };

        fcx.type_inference_fallback();

        // Even though coercion casts provide type hints, we check casts after fallback for
        // backwards compatibility. This makes fallback a stronger type hint than a cast coercion.
        fcx.check_casts();
        fcx.select_obligations_where_possible(|_| {});

        // Closure and generator analysis may run after fallback
        // because they don't constrain other type variables.
        // Closure analysis only runs on closures. Therefore they only need to fulfill non-const predicates (as of now)
        let prev_constness = fcx.param_env.constness();
        fcx.param_env = fcx.param_env.without_const();
        fcx.closure_analyze(body);
        fcx.param_env = fcx.param_env.with_constness(prev_constness);
        assert!(fcx.deferred_call_resolutions.borrow().is_empty());
        // Before the generator analysis, temporary scopes shall be marked to provide more
        // precise information on types to be captured.
        fcx.resolve_rvalue_scopes(def_id.to_def_id());
        fcx.resolve_generator_interiors(def_id.to_def_id());

        for (ty, span, code) in fcx.deferred_sized_obligations.borrow_mut().drain(..) {
            let ty = fcx.normalize_ty(span, ty);
            fcx.require_type_is_sized(ty, span, code);
        }

        fcx.select_all_obligations_or_error();

        if let None = fcx.infcx.tainted_by_errors() {
            fcx.check_transmutes();
        }

        fcx.check_asms();

        fcx.infcx.skip_region_resolution();

        fcx.resolve_type_vars_in_body(body)
    });

    // Consistency check our TypeckResults instance can hold all ItemLocalIds
    // it will need to hold.
    assert_eq!(typeck_results.hir_owner, id.owner);

    typeck_results
}

/// When `check_fn` is invoked on a generator (i.e., a body that
/// includes yield), it returns back some information about the yield
/// points.
struct GeneratorTypes<'tcx> {
    /// Type of generator argument / values returned by `yield`.
    resume_ty: Ty<'tcx>,

    /// Type of value that is yielded.
    yield_ty: Ty<'tcx>,

    /// Types that are captured (see `GeneratorInterior` for more).
    interior: Ty<'tcx>,

    /// Indicates if the generator is movable or static (immovable).
    movability: hir::Movability,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Needs {
    MutPlace,
    None,
}

impl Needs {
    fn maybe_mut_place(m: hir::Mutability) -> Self {
        match m {
            hir::Mutability::Mut => Needs::MutPlace,
            hir::Mutability::Not => Needs::None,
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum PlaceOp {
    Deref,
    Index,
}

pub struct BreakableCtxt<'tcx> {
    may_break: bool,

    // this is `null` for loops where break with a value is illegal,
    // such as `while`, `for`, and `while let`
    coerce: Option<DynamicCoerceMany<'tcx>>,
}

pub struct EnclosingBreakables<'tcx> {
    stack: Vec<BreakableCtxt<'tcx>>,
    by_id: HirIdMap<usize>,
}

impl<'tcx> EnclosingBreakables<'tcx> {
    fn find_breakable(&mut self, target_id: hir::HirId) -> &mut BreakableCtxt<'tcx> {
        self.opt_find_breakable(target_id).unwrap_or_else(|| {
            bug!("could not find enclosing breakable with id {}", target_id);
        })
    }

    fn opt_find_breakable(&mut self, target_id: hir::HirId) -> Option<&mut BreakableCtxt<'tcx>> {
        match self.by_id.get(&target_id) {
            Some(ix) => Some(&mut self.stack[*ix]),
            None => None,
        }
    }
}

fn report_unexpected_variant_res(
    tcx: TyCtxt<'_>,
    res: Res,
    qpath: &hir::QPath<'_>,
    span: Span,
    err_code: &str,
    expected: &str,
) -> ErrorGuaranteed {
    let res_descr = match res {
        Res::Def(DefKind::Variant, _) => "struct variant",
        _ => res.descr(),
    };
    let path_str = rustc_hir_pretty::qpath_to_string(qpath);
    let mut err = tcx.sess.struct_span_err_with_code(
        span,
        format!("expected {expected}, found {res_descr} `{path_str}`"),
        DiagnosticId::Error(err_code.into()),
    );
    match res {
        Res::Def(DefKind::Fn | DefKind::AssocFn, _) if err_code == "E0164" => {
            let patterns_url = "https://doc.rust-lang.org/book/ch18-00-patterns.html";
            err.span_label(span, "`fn` calls are not allowed in patterns");
            err.help(format!("for more information, visit {patterns_url}"))
        }
        _ => err.span_label(span, format!("not a {expected}")),
    }
    .emit()
}

/// Controls whether the arguments are tupled. This is used for the call
/// operator.
///
/// Tupling means that all call-side arguments are packed into a tuple and
/// passed as a single parameter. For example, if tupling is enabled, this
/// function:
/// ```
/// fn f(x: (isize, isize)) {}
/// ```
/// Can be called as:
/// ```ignore UNSOLVED (can this be done in user code?)
/// # fn f(x: (isize, isize)) {}
/// f(1, 2);
/// ```
/// Instead of:
/// ```
/// # fn f(x: (isize, isize)) {}
/// f((1, 2));
/// ```
#[derive(Copy, Clone, Eq, PartialEq)]
enum TupleArgumentsFlag {
    DontTupleArguments,
    TupleArguments,
}

fn fatally_break_rust(sess: &Session) {
    let handler = sess.diagnostic();
    handler.span_bug_no_panic(
        MultiSpan::new(),
        "It looks like you're trying to break rust; would you like some ICE?",
    );
    handler.note_without_error("the compiler expectedly panicked. this is a feature.");
    handler.note_without_error(
        "we would appreciate a joke overview: \
         https://github.com/rust-lang/rust/issues/43162#issuecomment-320764675",
    );
    handler.note_without_error(&format!(
        "rustc {} running on {}",
        option_env!("CFG_VERSION").unwrap_or("unknown_version"),
        config::host_triple(),
    ));
}

fn has_expected_num_generic_args(
    tcx: TyCtxt<'_>,
    trait_did: Option<DefId>,
    expected: usize,
) -> bool {
    trait_did.map_or(true, |trait_did| {
        let generics = tcx.generics_of(trait_did);
        generics.count() == expected + if generics.has_self { 1 } else { 0 }
    })
}

pub fn provide(providers: &mut Providers) {
    method::provide(providers);
    *providers = Providers {
        typeck_item_bodies,
        typeck_const_arg,
        typeck,
        diagnostic_only_typeck,
        has_typeck_results,
        used_trait_imports,
        ..*providers
    };
}
