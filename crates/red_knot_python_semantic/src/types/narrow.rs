use crate::semantic_index::ast_ids::HasScopedAstId;
use crate::semantic_index::constraint::{Constraint, ConstraintNode, PatternConstraint};
use crate::semantic_index::definition::Definition;
use crate::semantic_index::expression::Expression;
use crate::semantic_index::symbol::{ScopeId, ScopedSymbolId, SymbolTable};
use crate::semantic_index::symbol_table;
use crate::types::{
    infer_expression_types, IntersectionBuilder, KnownFunction, Type, UnionBuilder,
};
use crate::Db;
use itertools::Itertools;
use ruff_python_ast as ast;
use rustc_hash::FxHashMap;
use std::sync::Arc;

/// Return the type constraint that `test` (if true) would place on `definition`, if any.
///
/// For example, if we have this code:
///
/// ```python
/// y = 1 if flag else None
/// x = 1 if flag else None
/// if x is not None:
///     ...
/// ```
///
/// The `test` expression `x is not None` places the constraint "not None" on the definition of
/// `x`, so in that case we'd return `Some(Type::Intersection(negative=[Type::None]))`.
///
/// But if we called this with the same `test` expression, but the `definition` of `y`, no
/// constraint is applied to that definition, so we'd just return `None`.
pub(crate) fn narrowing_constraint<'db>(
    db: &'db dyn Db,
    constraint: Constraint<'db>,
    definition: Definition<'db>,
) -> Option<Type<'db>> {
    match constraint.node {
        ConstraintNode::Expression(expression) => {
            if constraint.is_positive {
                all_narrowing_constraints_for_expression(db, expression)
                    .get(&definition.symbol(db))
                    .copied()
            } else {
                all_negative_narrowing_constraints_for_expression(db, expression)
                    .get(&definition.symbol(db))
                    .copied()
            }
        }
        ConstraintNode::Pattern(pattern) => all_narrowing_constraints_for_pattern(db, pattern)
            .get(&definition.symbol(db))
            .copied(),
    }
}

#[salsa::tracked(return_ref)]
fn all_narrowing_constraints_for_pattern<'db>(
    db: &'db dyn Db,
    pattern: PatternConstraint<'db>,
) -> NarrowingConstraints<'db> {
    NarrowingConstraintsBuilder::new(db, ConstraintNode::Pattern(pattern), true).finish()
}

#[salsa::tracked(return_ref)]
fn all_narrowing_constraints_for_expression<'db>(
    db: &'db dyn Db,
    expression: Expression<'db>,
) -> NarrowingConstraints<'db> {
    NarrowingConstraintsBuilder::new(db, ConstraintNode::Expression(expression), true).finish()
}

#[salsa::tracked(return_ref)]
fn all_negative_narrowing_constraints_for_expression<'db>(
    db: &'db dyn Db,
    expression: Expression<'db>,
) -> NarrowingConstraints<'db> {
    NarrowingConstraintsBuilder::new(db, ConstraintNode::Expression(expression), false).finish()
}

/// Generate a constraint from the *type* of the second argument of an `isinstance` call.
///
/// Example: for `isinstance(…, str)`, we would infer `Type::ClassLiteral(str)` from the
/// second argument, but we need to generate a `Type::Instance(str)` constraint that can
/// be used to narrow down the type of the first argument.
fn generate_isinstance_constraint<'db>(
    db: &'db dyn Db,
    classinfo: &Type<'db>,
) -> Option<Type<'db>> {
    match classinfo {
        Type::ClassLiteral(class) => Some(Type::Instance(*class)),
        Type::Tuple(tuple) => {
            let mut builder = UnionBuilder::new(db);
            for element in tuple.elements(db) {
                builder = builder.add(generate_isinstance_constraint(db, element)?);
            }
            Some(builder.build())
        }
        _ => None,
    }
}

type NarrowingConstraints<'db> = FxHashMap<ScopedSymbolId, Type<'db>>;

struct NarrowingConstraintsBuilder<'db> {
    db: &'db dyn Db,
    constraint: ConstraintNode<'db>,
    is_positive: bool,
    constraints: NarrowingConstraints<'db>,
}

impl<'db> NarrowingConstraintsBuilder<'db> {
    fn new(db: &'db dyn Db, constraint: ConstraintNode<'db>, is_positive: bool) -> Self {
        Self {
            db,
            constraint,
            is_positive,
            constraints: NarrowingConstraints::default(),
        }
    }

    fn finish(mut self) -> NarrowingConstraints<'db> {
        match self.constraint {
            ConstraintNode::Expression(expression) => {
                self.evaluate_expression_constraint(expression, self.is_positive);
            }
            ConstraintNode::Pattern(pattern) => self.evaluate_pattern_constraint(pattern),
        }
        self.constraints.shrink_to_fit();
        self.constraints
    }

    fn evaluate_expression_constraint(&mut self, expression: Expression<'db>, is_positive: bool) {
        let expression_node = expression.node_ref(self.db).node();
        self.evaluate_expression_node_constraint(expression_node, expression, is_positive);
    }

    fn evaluate_expression_node_constraint(
        &mut self,
        expression_node: &ruff_python_ast::Expr,
        expression: Expression<'db>,
        is_positive: bool,
    ) {
        match expression_node {
            ast::Expr::Compare(expr_compare) => {
                self.add_expr_compare(expr_compare, expression, is_positive);
            }
            ast::Expr::Call(expr_call) => {
                self.add_expr_call(expr_call, expression, is_positive);
            }
            ast::Expr::UnaryOp(unary_op) if unary_op.op == ast::UnaryOp::Not => {
                self.evaluate_expression_node_constraint(
                    &unary_op.operand,
                    expression,
                    !is_positive,
                );
            }
            _ => {} // TODO other test expression kinds
        }
    }

    fn evaluate_pattern_constraint(&mut self, pattern: PatternConstraint<'db>) {
        let subject = pattern.subject(self.db);

        match pattern.pattern(self.db).node() {
            ast::Pattern::MatchValue(_) => {
                // TODO
            }
            ast::Pattern::MatchSingleton(singleton_pattern) => {
                self.add_match_pattern_singleton(subject, singleton_pattern);
            }
            ast::Pattern::MatchSequence(_) => {
                // TODO
            }
            ast::Pattern::MatchMapping(_) => {
                // TODO
            }
            ast::Pattern::MatchClass(_) => {
                // TODO
            }
            ast::Pattern::MatchStar(_) => {
                // TODO
            }
            ast::Pattern::MatchAs(_) => {
                // TODO
            }
            ast::Pattern::MatchOr(_) => {
                // TODO
            }
        }
    }

    fn symbols(&self) -> Arc<SymbolTable> {
        symbol_table(self.db, self.scope())
    }

    fn scope(&self) -> ScopeId<'db> {
        match self.constraint {
            ConstraintNode::Expression(expression) => expression.scope(self.db),
            ConstraintNode::Pattern(pattern) => pattern.scope(self.db),
        }
    }

    fn add_expr_compare(
        &mut self,
        expr_compare: &ast::ExprCompare,
        expression: Expression<'db>,
        is_positive: bool,
    ) {
        let ast::ExprCompare {
            range: _,
            left,
            ops,
            comparators,
        } = expr_compare;
        if !left.is_name_expr() && comparators.iter().all(|c| !c.is_name_expr()) {
            // If none of the comparators are name expressions,
            // we have no symbol to narrow down the type of.
            return;
        }
        if !is_positive && comparators.len() > 1 {
            // We can't negate a constraint made by a multi-comparator expression, since we can't
            // know which comparison part is the one being negated.
            // For example, the negation of  `x is 1 is y is 2`, would be `(x is not 1) or (y is not 1) or (y is not 2)`
            // and that requires cross-symbol constraints, which we don't support yet.
            return;
        }
        let scope = self.scope();
        let inference = infer_expression_types(self.db, expression);

        let comparator_tuples = std::iter::once(&**left)
            .chain(comparators)
            .tuple_windows::<(&ruff_python_ast::Expr, &ruff_python_ast::Expr)>();
        for (op, (left, right)) in std::iter::zip(&**ops, comparator_tuples) {
            if let ast::Expr::Name(ast::ExprName {
                range: _,
                id,
                ctx: _,
            }) = left
            {
                // SAFETY: we should always have a symbol for every Name node.
                let symbol = self.symbols().symbol_id_by_name(id).unwrap();
                let rhs_ty = inference.expression_ty(right.scoped_ast_id(self.db, scope));

                match if is_positive { *op } else { op.negate() } {
                    ast::CmpOp::IsNot => {
                        if rhs_ty.is_singleton() {
                            let ty = IntersectionBuilder::new(self.db)
                                .add_negative(rhs_ty)
                                .build();
                            self.constraints.insert(symbol, ty);
                        } else {
                            // Non-singletons cannot be safely narrowed using `is not`
                        }
                    }
                    ast::CmpOp::Is => {
                        self.constraints.insert(symbol, rhs_ty);
                    }
                    ast::CmpOp::NotEq => {
                        if rhs_ty.is_single_valued(self.db) {
                            let ty = IntersectionBuilder::new(self.db)
                                .add_negative(rhs_ty)
                                .build();
                            self.constraints.insert(symbol, ty);
                        }
                    }
                    _ => {
                        // TODO other comparison types
                    }
                }
            }
        }
    }

    fn add_expr_call(
        &mut self,
        expr_call: &ast::ExprCall,
        expression: Expression<'db>,
        is_positive: bool,
    ) {
        let scope = self.scope();
        let inference = infer_expression_types(self.db, expression);

        if let Some(func_type) = inference
            .expression_ty(expr_call.func.scoped_ast_id(self.db, scope))
            .into_function_literal_type()
        {
            if func_type.is_known(self.db, KnownFunction::IsInstance)
                && expr_call.arguments.keywords.is_empty()
            {
                if let [ast::Expr::Name(ast::ExprName { id, .. }), rhs] = &*expr_call.arguments.args
                {
                    let symbol = self.symbols().symbol_id_by_name(id).unwrap();

                    let rhs_type = inference.expression_ty(rhs.scoped_ast_id(self.db, scope));

                    // TODO: add support for PEP 604 union types on the right hand side:
                    // isinstance(x, str | (int | float))
                    if let Some(mut constraint) = generate_isinstance_constraint(self.db, &rhs_type)
                    {
                        if !is_positive {
                            constraint = constraint.negate(self.db);
                        }
                        self.constraints.insert(symbol, constraint);
                    }
                }
            }
        }
    }

    fn add_match_pattern_singleton(
        &mut self,
        subject: &ast::Expr,
        pattern: &ast::PatternMatchSingleton,
    ) {
        if let Some(ast::ExprName { id, .. }) = subject.as_name_expr() {
            // SAFETY: we should always have a symbol for every Name node.
            let symbol = self.symbols().symbol_id_by_name(id).unwrap();

            let ty = match pattern.value {
                ast::Singleton::None => Type::None,
                ast::Singleton::True => Type::BooleanLiteral(true),
                ast::Singleton::False => Type::BooleanLiteral(false),
            };
            self.constraints.insert(symbol, ty);
        }
    }
}
