use crate::no_std::prelude::*;

use crate::ast::Span;
use crate::compile::context::ContextMeta;
use crate::compile::ir;
use crate::compile::meta;
use crate::compile::{
    self, Assembly, CompileErrorKind, IrBudget, IrCompiler, IrInterpreter, ItemId, ItemMeta,
    Location, Options, QueryErrorKind, WithSpan,
};
use crate::hir;
use crate::query::{Named, Query, QueryConstFn, Used};
use crate::runtime::{ConstValue, Inst};
use crate::{Context, Diagnostics, Hash, SourceId};

pub(crate) mod assemble;
mod loops;
mod scopes;

pub(crate) use self::loops::{Loop, Loops};
pub(crate) use self::scopes::{Scope, ScopeGuard, Scopes, Var};

/// A needs hint for an expression.
/// This is used to contextually determine what an expression is expected to
/// produce.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Needs {
    Type,
    Value,
    None,
}

impl Needs {
    /// Test if any sort of value is needed.
    pub(crate) fn value(self) -> bool {
        matches!(self, Self::Type | Self::Value)
    }
}

pub(crate) struct Assembler<'a> {
    /// The source id of the source.
    pub(crate) source_id: SourceId,
    /// The context we are compiling for.
    pub(crate) context: &'a Context,
    /// Query system to compile required items.
    pub(crate) q: Query<'a>,
    /// The assembly we are generating.
    pub(crate) asm: &'a mut Assembly,
    /// Scopes defined in the compiler.
    pub(crate) scopes: Scopes,
    /// Context for which to emit warnings.
    pub(crate) contexts: Vec<Span>,
    /// The nesting of loop we are currently in.
    pub(crate) loops: Loops,
    /// Enabled optimizations.
    pub(crate) options: &'a Options,
    /// Compilation warnings.
    pub(crate) diagnostics: &'a mut Diagnostics,
}

impl<'a> Assembler<'a> {
    // Pick private metadata to compile for the item.
    fn select_context_meta<'m>(
        &self,
        item: ItemId,
        metas: impl Iterator<Item = &'m ContextMeta> + Clone,
        parameters: Option<Hash>,
    ) -> Result<Option<&'m ContextMeta>, Box<QueryErrorKind>> {
        let parameters = parameters.unwrap_or(Hash::EMPTY);

        let metas2 = metas.clone();

        // If there is a single item matching the specified generic hash, pick
        // it.
        let mut it = metas2
            .filter(|i| !matches!(i.kind, meta::Kind::Macro | meta::Kind::Module))
            .filter(|i| i.kind.as_parameters() == parameters);

        if let Some(meta) = it.next() {
            if it.next().is_none() {
                return Ok(Some(meta));
            }
        } else {
            return Ok(None);
        }

        Err(Box::new(QueryErrorKind::AmbiguousContextItem {
            item: self.q.pool.item(item).to_owned(),
            infos: metas.map(|i| i.info()).collect(),
        }))
    }

    /// Access the meta for the given language item.
    pub fn try_lookup_meta(
        &mut self,
        span: Span,
        item: ItemId,
        generics: Option<Hash>,
    ) -> compile::Result<Option<meta::Meta>> {
        tracing::trace!("lookup meta: {:?}", item);

        if generics.is_none() {
            if let Some(meta) = self.q.query_meta(span, item, Default::default())? {
                tracing::trace!("found in query: {:?}", meta);
                self.q.visitor.visit_meta(
                    Location::new(self.source_id, span),
                    meta.as_meta_ref(self.q.pool),
                );
                return Ok(Some(meta));
            }
        }

        let metas = self.context.lookup_meta(self.q.pool.item(item));

        let Some(meta) = self.select_context_meta(item, metas, generics).with_span(span)? else {
            return Ok(None);
        };

        let meta = self.q.insert_context_meta(span, meta)?;

        tracing::trace!("Found in context: {:?}", meta);

        self.q.visitor.visit_meta(
            Location::new(self.source_id, span),
            meta.as_meta_ref(self.q.pool),
        );

        Ok(Some(meta))
    }

    /// Access the meta for the given language item.
    pub fn lookup_meta(
        &mut self,
        span: Span,
        item: ItemId,
        parameters: Option<Hash>,
    ) -> compile::Result<meta::Meta> {
        if let Some(meta) = self.try_lookup_meta(span, item, parameters)? {
            return Ok(meta);
        }

        let kind = if let Some(parameters) = parameters {
            CompileErrorKind::MissingItemParameters {
                item: self.q.pool.item(item).to_owned(),
                parameters,
            }
        } else {
            CompileErrorKind::MissingItem {
                item: self.q.pool.item(item).to_owned(),
            }
        };

        Err(compile::Error::new(span, kind))
    }

    /// Pop locals by simply popping them.
    pub(crate) fn locals_pop(&mut self, total_var_count: usize, span: Span) {
        match total_var_count {
            0 => (),
            1 => {
                self.asm.push(Inst::Pop, span);
            }
            count => {
                self.asm.push(Inst::PopN { count }, span);
            }
        }
    }

    /// Clean up local variables by preserving the value that is on top and
    /// popping the rest.
    ///
    /// The clean operation will preserve the value that is on top of the stack,
    /// and pop the values under it.
    pub(crate) fn locals_clean(&mut self, total_var_count: usize, span: Span) {
        match total_var_count {
            0 => (),
            count => {
                self.asm.push(Inst::Clean { count }, span);
            }
        }
    }

    /// Convert an [ast::Path] into a [Named] item.
    pub(crate) fn convert_path<'hir>(
        &mut self,
        path: &'hir hir::Path<'hir>,
    ) -> compile::Result<Named<'hir>> {
        self.q.convert_path(self.context, path)
    }

    /// Clean the last scope.
    pub(crate) fn clean_last_scope(
        &mut self,
        span: Span,
        expected: ScopeGuard,
        needs: Needs,
    ) -> compile::Result<()> {
        let scope = self.scopes.pop(expected, span)?;

        if needs.value() {
            self.locals_clean(scope.local_var_count, span);
        } else {
            self.locals_pop(scope.local_var_count, span);
        }

        Ok(())
    }

    /// Get the latest relevant warning context.
    pub(crate) fn context(&self) -> Option<Span> {
        self.contexts.last().copied()
    }

    /// Calling a constant function by id and return the resuling value.
    pub(crate) fn call_const_fn(
        &mut self,
        span: Span,
        meta: &meta::Meta,
        from: &ItemMeta,
        query_const_fn: &QueryConstFn,
        args: &[hir::Expr<'_>],
    ) -> compile::Result<ConstValue> {
        if query_const_fn.ir_fn.args.len() != args.len() {
            return Err(compile::Error::new(
                span,
                CompileErrorKind::UnsupportedArgumentCount {
                    meta: meta.info(self.q.pool),
                    expected: query_const_fn.ir_fn.args.len(),
                    actual: args.len(),
                },
            ));
        }

        let mut compiler = IrCompiler {
            source_id: self.source_id,
            q: self.q.borrow(),
        };

        let mut compiled = Vec::new();

        // TODO: precompile these and fetch using opaque id?
        for (hir, name) in args.iter().zip(&query_const_fn.ir_fn.args) {
            compiled.push((ir::compiler::expr(hir, &mut compiler)?, name));
        }

        let mut interpreter = IrInterpreter {
            budget: IrBudget::new(1_000_000),
            scopes: Default::default(),
            module: from.module,
            item: from.item,
            q: self.q.borrow(),
        };

        for (ir, name) in compiled {
            let value = interpreter.eval_value(&ir, Used::Used)?;
            interpreter.scopes.decl(name, value).with_span(span)?;
        }

        interpreter.module = query_const_fn.item_meta.module;
        interpreter.item = query_const_fn.item_meta.item;
        let value = interpreter.eval_value(&query_const_fn.ir_fn.ir, Used::Used)?;
        value.into_const(span)
    }
}