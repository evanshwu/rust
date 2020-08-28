//! A solver for dataflow problems.

use std::borrow::BorrowMut;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use rustc_ast as ast;
use rustc_data_structures::work_queue::WorkQueue;
use rustc_graphviz as dot;
use rustc_hir::def_id::DefId;
use rustc_index::bit_set::BitSet;
use rustc_index::vec::{Idx, IndexVec};
use rustc_middle::mir::{self, traversal, BasicBlock};
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::symbol::{sym, Symbol};

use super::fmt::DebugWithContext;
use super::graphviz;
use super::{
    visit_results, Analysis, Direction, GenKill, GenKillAnalysis, GenKillSet, JoinSemiLattice,
    ResultsCursor, ResultsVisitor,
};
use crate::util::pretty::dump_enabled;

/// A dataflow analysis that has converged to fixpoint.
pub struct Results<'tcx, A>
where
    A: Analysis<'tcx>,
{
    pub analysis: A,
    pub(super) entry_sets: IndexVec<BasicBlock, A::Domain>,
}

impl<A> Results<'tcx, A>
where
    A: Analysis<'tcx>,
{
    /// Creates a `ResultsCursor` that can inspect these `Results`.
    pub fn into_results_cursor(self, body: &'mir mir::Body<'tcx>) -> ResultsCursor<'mir, 'tcx, A> {
        ResultsCursor::new(body, self)
    }

    /// Gets the dataflow state for the given block.
    pub fn entry_set_for_block(&self, block: BasicBlock) -> &A::Domain {
        &self.entry_sets[block]
    }

    pub fn visit_with(
        &self,
        body: &'mir mir::Body<'tcx>,
        blocks: impl IntoIterator<Item = BasicBlock>,
        vis: &mut impl ResultsVisitor<'mir, 'tcx, FlowState = A::Domain>,
    ) {
        visit_results(body, blocks, self, vis)
    }

    pub fn visit_reachable_with(
        &self,
        body: &'mir mir::Body<'tcx>,
        vis: &mut impl ResultsVisitor<'mir, 'tcx, FlowState = A::Domain>,
    ) {
        let blocks = mir::traversal::reachable(body);
        visit_results(body, blocks.map(|(bb, _)| bb), self, vis)
    }

    pub fn visit_in_rpo_with(
        &self,
        body: &'mir mir::Body<'tcx>,
        vis: &mut impl ResultsVisitor<'mir, 'tcx, FlowState = A::Domain>,
    ) {
        let blocks = mir::traversal::reverse_postorder(body);
        visit_results(body, blocks.map(|(bb, _)| bb), self, vis)
    }
}

/// A solver for dataflow problems.
pub struct Engine<'a, 'tcx, A>
where
    A: Analysis<'tcx>,
{
    tcx: TyCtxt<'tcx>,
    body: &'a mir::Body<'tcx>,
    def_id: DefId,
    dead_unwinds: Option<&'a BitSet<BasicBlock>>,
    entry_sets: IndexVec<BasicBlock, A::Domain>,
    analysis: A,

    /// Cached, cumulative transfer functions for each block.
    apply_trans_for_block: Option<Box<dyn Fn(BasicBlock, &mut A::Domain)>>,
}

impl<A, D, T> Engine<'a, 'tcx, A>
where
    A: GenKillAnalysis<'tcx, Idx = T, Domain = D>,
    D: Clone + JoinSemiLattice + GenKill<T> + BorrowMut<BitSet<T>>,
    T: Idx,
{
    /// Creates a new `Engine` to solve a gen-kill dataflow problem.
    pub fn new_gen_kill(
        tcx: TyCtxt<'tcx>,
        body: &'a mir::Body<'tcx>,
        def_id: DefId,
        analysis: A,
    ) -> Self {
        // If there are no back-edges in the control-flow graph, we only ever need to apply the
        // transfer function for each block exactly once (assuming that we process blocks in RPO).
        //
        // In this case, there's no need to compute the block transfer functions ahead of time.
        if !body.is_cfg_cyclic() {
            return Self::new(tcx, body, def_id, analysis, None);
        }

        // Otherwise, compute and store the cumulative transfer function for each block.

        let identity = GenKillSet::identity(analysis.bottom_value(body).borrow().domain_size());
        let mut trans_for_block = IndexVec::from_elem(identity, body.basic_blocks());

        for (block, block_data) in body.basic_blocks().iter_enumerated() {
            let trans = &mut trans_for_block[block];
            A::Direction::gen_kill_effects_in_block(&analysis, trans, block, block_data);
        }

        let apply_trans = Box::new(move |bb: BasicBlock, state: &mut A::Domain| {
            trans_for_block[bb].apply(state.borrow_mut());
        });

        Self::new(tcx, body, def_id, analysis, Some(apply_trans as Box<_>))
    }
}

impl<A, D> Engine<'a, 'tcx, A>
where
    A: Analysis<'tcx, Domain = D>,
    D: Clone + JoinSemiLattice,
{
    /// Creates a new `Engine` to solve a dataflow problem with an arbitrary transfer
    /// function.
    ///
    /// Gen-kill problems should use `new_gen_kill`, which will coalesce transfer functions for
    /// better performance.
    pub fn new_generic(
        tcx: TyCtxt<'tcx>,
        body: &'a mir::Body<'tcx>,
        def_id: DefId,
        analysis: A,
    ) -> Self {
        Self::new(tcx, body, def_id, analysis, None)
    }

    fn new(
        tcx: TyCtxt<'tcx>,
        body: &'a mir::Body<'tcx>,
        def_id: DefId,
        analysis: A,
        apply_trans_for_block: Option<Box<dyn Fn(BasicBlock, &mut A::Domain)>>,
    ) -> Self {
        let bottom_value = analysis.bottom_value(body);
        let mut entry_sets = IndexVec::from_elem(bottom_value.clone(), body.basic_blocks());
        analysis.initialize_start_block(body, &mut entry_sets[mir::START_BLOCK]);

        if A::Direction::is_backward() && entry_sets[mir::START_BLOCK] != bottom_value {
            bug!("`initialize_start_block` is not yet supported for backward dataflow analyses");
        }

        Engine {
            analysis,
            tcx,
            body,
            def_id,
            dead_unwinds: None,
            entry_sets,
            apply_trans_for_block,
        }
    }

    /// Signals that we do not want dataflow state to propagate across unwind edges for these
    /// `BasicBlock`s.
    ///
    /// You must take care that `dead_unwinds` does not contain a `BasicBlock` that *can* actually
    /// unwind during execution. Otherwise, your dataflow results will not be correct.
    pub fn dead_unwinds(mut self, dead_unwinds: &'a BitSet<BasicBlock>) -> Self {
        self.dead_unwinds = Some(dead_unwinds);
        self
    }

    /// Computes the fixpoint for this dataflow problem and returns it.
    pub fn iterate_to_fixpoint(self) -> Results<'tcx, A>
    where
        A::Domain: DebugWithContext<A>,
    {
        let Engine {
            analysis,
            body,
            dead_unwinds,
            def_id,
            mut entry_sets,
            tcx,
            apply_trans_for_block,
            ..
        } = self;

        let mut dirty_queue: WorkQueue<BasicBlock> =
            WorkQueue::with_none(body.basic_blocks().len());

        if A::Direction::is_forward() {
            for (bb, _) in traversal::reverse_postorder(body) {
                dirty_queue.insert(bb);
            }
        } else {
            // Reverse post-order on the reverse CFG may generate a better iteration order for
            // backward dataflow analyses, but probably not enough to matter.
            for (bb, _) in traversal::postorder(body) {
                dirty_queue.insert(bb);
            }
        }

        let mut state = analysis.bottom_value(body);
        while let Some(bb) = dirty_queue.pop() {
            let bb_data = &body[bb];

            // Apply the block transfer function, using the cached one if it exists.
            state.clone_from(&entry_sets[bb]);
            match &apply_trans_for_block {
                Some(apply) => apply(bb, &mut state),
                None => A::Direction::apply_effects_in_block(&analysis, &mut state, bb, bb_data),
            }

            A::Direction::join_state_into_successors_of(
                &analysis,
                tcx,
                body,
                dead_unwinds,
                &mut state,
                (bb, bb_data),
                |target: BasicBlock, state: &A::Domain| {
                    let set_changed = entry_sets[target].join(state);
                    if set_changed {
                        dirty_queue.insert(target);
                    }
                },
            );
        }

        let results = Results { analysis, entry_sets };

        let res = write_graphviz_results(tcx, def_id, &body, &results);
        if let Err(e) = res {
            warn!("Failed to write graphviz dataflow results: {}", e);
        }

        results
    }
}

// Graphviz

/// Writes a DOT file containing the results of a dataflow analysis if the user requested it via
/// `rustc_mir` attributes.
fn write_graphviz_results<A>(
    tcx: TyCtxt<'tcx>,
    def_id: DefId,
    body: &mir::Body<'tcx>,
    results: &Results<'tcx, A>,
) -> std::io::Result<()>
where
    A: Analysis<'tcx>,
    A::Domain: DebugWithContext<A>,
{
    let attrs = match RustcMirAttrs::parse(tcx, def_id) {
        Ok(attrs) => attrs,

        // Invalid `rustc_mir` attrs are reported in `RustcMirAttrs::parse`
        Err(()) => return Ok(()),
    };

    let path = match attrs.output_path(A::NAME) {
        Some(path) => path,

        None if tcx.sess.opts.debugging_opts.dump_mir_dataflow
            && dump_enabled(tcx, A::NAME, def_id) =>
        {
            let mut path = PathBuf::from(&tcx.sess.opts.debugging_opts.dump_mir_dir);

            let item_name = ty::print::with_forced_impl_filename_line(|| {
                tcx.def_path(def_id).to_filename_friendly_no_crate()
            });
            path.push(format!("rustc.{}.{}.dot", item_name, A::NAME));
            path
        }

        None => return Ok(()),
    };

    let style = match attrs.formatter {
        Some(sym::two_phase) => graphviz::OutputStyle::BeforeAndAfter,
        _ => graphviz::OutputStyle::AfterOnly,
    };

    debug!("printing dataflow results for {:?} to {}", def_id, path.display());
    let mut buf = Vec::new();

    let graphviz = graphviz::Formatter::new(body, def_id, results, style);
    dot::render_opts(&graphviz, &mut buf, &[dot::RenderOption::Monospace])?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, buf)?;

    Ok(())
}

#[derive(Default)]
struct RustcMirAttrs {
    basename_and_suffix: Option<PathBuf>,
    formatter: Option<Symbol>,
}

impl RustcMirAttrs {
    fn parse(tcx: TyCtxt<'tcx>, def_id: DefId) -> Result<Self, ()> {
        let attrs = tcx.get_attrs(def_id);

        let mut result = Ok(());
        let mut ret = RustcMirAttrs::default();

        let rustc_mir_attrs = attrs
            .iter()
            .filter(|attr| tcx.sess.check_name(attr, sym::rustc_mir))
            .flat_map(|attr| attr.meta_item_list().into_iter().flat_map(|v| v.into_iter()));

        for attr in rustc_mir_attrs {
            let attr_result = if attr.has_name(sym::borrowck_graphviz_postflow) {
                Self::set_field(&mut ret.basename_and_suffix, tcx, &attr, |s| {
                    let path = PathBuf::from(s.to_string());
                    match path.file_name() {
                        Some(_) => Ok(path),
                        None => {
                            tcx.sess.span_err(attr.span(), "path must end in a filename");
                            Err(())
                        }
                    }
                })
            } else if attr.has_name(sym::borrowck_graphviz_format) {
                Self::set_field(&mut ret.formatter, tcx, &attr, |s| match s {
                    sym::gen_kill | sym::two_phase => Ok(s),
                    _ => {
                        tcx.sess.span_err(attr.span(), "unknown formatter");
                        Err(())
                    }
                })
            } else {
                Ok(())
            };

            result = result.and(attr_result);
        }

        result.map(|()| ret)
    }

    fn set_field<T>(
        field: &mut Option<T>,
        tcx: TyCtxt<'tcx>,
        attr: &ast::NestedMetaItem,
        mapper: impl FnOnce(Symbol) -> Result<T, ()>,
    ) -> Result<(), ()> {
        if field.is_some() {
            tcx.sess
                .span_err(attr.span(), &format!("duplicate values for `{}`", attr.name_or_empty()));

            return Err(());
        }

        if let Some(s) = attr.value_str() {
            *field = Some(mapper(s)?);
            Ok(())
        } else {
            tcx.sess
                .span_err(attr.span(), &format!("`{}` requires an argument", attr.name_or_empty()));
            Err(())
        }
    }

    /// Returns the path where dataflow results should be written, or `None`
    /// `borrowck_graphviz_postflow` was not specified.
    ///
    /// This performs the following transformation to the argument of `borrowck_graphviz_postflow`:
    ///
    /// "path/suffix.dot" -> "path/analysis_name_suffix.dot"
    fn output_path(&self, analysis_name: &str) -> Option<PathBuf> {
        let mut ret = self.basename_and_suffix.as_ref().cloned()?;
        let suffix = ret.file_name().unwrap(); // Checked when parsing attrs

        let mut file_name: OsString = analysis_name.into();
        file_name.push("_");
        file_name.push(suffix);
        ret.set_file_name(file_name);

        Some(ret)
    }
}
