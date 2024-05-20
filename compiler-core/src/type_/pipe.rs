use self::expression::CallKind;

use super::*;
use crate::ast::{Assignment, AssignmentKind, TypedAssignment, UntypedExpr, PIPE_VARIABLE};
use vec1::Vec1;

#[derive(Debug)]
pub(crate) struct PipeTyper<'a, 'b, 'c> {
    size: usize,
    argument_type: Arc<Type>,
    argument_location: SrcSpan,
    location: SrcSpan,
    assignments: Vec<TypedAssignment>,
    expr_typer: &'a mut ExprTyper<'b, 'c>,
}

impl<'a, 'b, 'c> PipeTyper<'a, 'b, 'c> {
    pub fn infer(
        expr_typer: &'a mut ExprTyper<'b, 'c>,
        expressions: Vec1<UntypedExpr>,
    ) -> Result<TypedExpr, Error> {
        // The scope is reset as pipelines are rewritten into a series of
        // assignments, and we don't want these variables to leak out of the
        // pipeline.
        let scope = expr_typer.environment.scope.clone();
        let result = PipeTyper::run(expr_typer, expressions);
        expr_typer.environment.scope = scope;
        result
    }

    fn run(
        expr_typer: &'a mut ExprTyper<'b, 'c>,
        expressions: Vec1<UntypedExpr>,
    ) -> Result<TypedExpr, Error> {
        let size = expressions.len();
        let end = &expressions[..]
            .last()
            // The vec is non-empty, this indexing can never fail
            .expect("Empty pipeline in typer")
            .location()
            .end;
        let mut expressions = expressions.into_iter();
        let first = expr_typer.infer(expressions.next().expect("Empty pipeline in typer"))?;
        let mut typer = Self {
            size,
            expr_typer,
            argument_type: first.type_(),
            argument_location: first.location(),
            location: SrcSpan {
                start: first.location().start,
                end: *end,
            },
            assignments: Vec::with_capacity(size),
        };

        let first_location = first.location();

        // No need to update self.argument_* as we set it above
        typer.push_assignment_no_update(first);

        // Perform the type checking
        typer.infer_expressions(expressions, first_location)
    }

    fn infer_expressions(
        &mut self,
        expressions: impl IntoIterator<Item = UntypedExpr>,
        first_location: SrcSpan,
    ) -> Result<TypedExpr, Error> {
        let finally = self.infer_each_expression(expressions, first_location);

        // Return any errors after clean-up
        let finally = finally?;
        let assignments = std::mem::take(&mut self.assignments);

        Ok(TypedExpr::Pipeline {
            assignments,
            location: self.location,
            finally: Box::new(finally),
        })
    }

    fn infer_each_expression(
        &mut self,
        expressions: impl IntoIterator<Item = UntypedExpr>,
        first_location: SrcSpan,
    ) -> Result<TypedExpr, Error> {
        let mut finally = None;
        let expressions = expressions.into_iter().collect_vec();
        let mut previous_expression_location: Option<SrcSpan> = None;

        for (i, call) in expressions.into_iter().enumerate() {
            self.warn_if_is_todo_or_panic(&call, first_location, previous_expression_location);
            if self.expr_typer.previous_panics {
                self.expr_typer
                    .warn_for_unreachable_code(call.location(), PanicPosition::PreviousExpression);
            }

            let call = match call {
                // left |> right(..args)
                UntypedExpr::Call {
                    fun,
                    arguments,
                    location,
                    ..
                } => {
                    let fun = self.expr_typer.infer(*fun)?;
                    match fun.type_().fn_arity() {
                        // Rewrite as right(left, ..args)
                        Some(arity) if arity == arguments.len() + 1 => {
                            self.infer_insert_pipe(fun, arguments, location)?
                        }

                        // Rewrite as right(..args)(left)
                        _ => self.infer_apply_to_call_pipe(fun, arguments, location)?,
                    }
                }

                // right(left)
                call => self.infer_apply_pipe(call)?,
            };

            previous_expression_location = Some(call.location());

            if i + 2 == self.size {
                finally = Some(call);
            } else {
                self.push_assignment(call);
            }
        }
        Ok(finally.expect("Empty pipeline in typer"))
    }

    /// Create a call argument that can be used to refer to the value on the
    /// left hand side of the pipe
    fn typed_left_hand_value_variable_call_argument(&self) -> CallArg<TypedExpr> {
        CallArg {
            label: None,
            location: self.argument_location,
            value: self.typed_left_hand_value_variable(),
            // This argument is given implicitly by the pipe, not explicitly by
            // the programmer.
            implicit: true,
        }
    }

    /// Create a call argument that can be used to refer to the value on the
    /// left hand side of the pipe
    fn untyped_left_hand_value_variable_call_argument(&self) -> CallArg<UntypedExpr> {
        CallArg {
            label: None,
            location: self.argument_location,
            value: self.untyped_left_hand_value_variable(),
            // This argument is given implicitly by the pipe, not explicitly by
            // the programmer.
            implicit: true,
        }
    }

    /// Create a variable that can be used to refer to the value on the left
    /// hand side of the pipe
    fn typed_left_hand_value_variable(&self) -> TypedExpr {
        TypedExpr::Var {
            location: self.argument_location,
            name: PIPE_VARIABLE.into(),
            constructor: ValueConstructor {
                publicity: Publicity::Public,
                deprecation: Deprecation::NotDeprecated,
                type_: self.argument_type.clone(),
                variant: ValueConstructorVariant::LocalVariable {
                    location: self.argument_location,
                },
            },
        }
    }

    /// Create a variable that can be used to refer to the value on the left
    /// hand side of the pipe
    fn untyped_left_hand_value_variable(&self) -> UntypedExpr {
        UntypedExpr::Var {
            location: self.argument_location,
            name: PIPE_VARIABLE.into(),
        }
    }

    /// Push an assignment for the value on the left hand side of the pipe
    fn push_assignment(&mut self, expression: TypedExpr) {
        self.argument_type = expression.type_();
        self.argument_location = expression.location();
        self.push_assignment_no_update(expression)
    }

    fn push_assignment_no_update(&mut self, expression: TypedExpr) {
        let location = expression.location();
        // Insert the variable for use in type checking the rest of the pipeline
        self.expr_typer.environment.insert_local_variable(
            PIPE_VARIABLE.into(),
            location,
            expression.type_(),
        );
        // Add the assignment to the AST
        let assignment = Assignment {
            location,
            annotation: None,
            kind: AssignmentKind::Let,
            pattern: Pattern::Variable {
                location,
                name: PIPE_VARIABLE.into(),
                type_: expression.type_(),
            },
            value: Box::new(expression),
        };
        self.assignments.push(assignment);
    }

    /// Attempt to infer a |> b(..c) as b(..c)(a)
    fn infer_apply_to_call_pipe(
        &mut self,
        function: TypedExpr,
        args: Vec<CallArg<UntypedExpr>>,
        location: SrcSpan,
    ) -> Result<TypedExpr, Error> {
        let (function, args, typ) = self.expr_typer.do_infer_call_with_known_fun(
            function,
            args,
            location,
            CallKind::Function,
        )?;
        let function = TypedExpr::Call {
            location,
            typ,
            args,
            fun: Box::new(function),
        };
        let args = vec![self.untyped_left_hand_value_variable_call_argument()];
        // TODO: use `.with_unify_error_situation(UnifyErrorSituation::PipeTypeMismatch)`
        // This will require the typing of the arguments to be lifted up out of
        // the function below. If it is not we don't know if the error comes
        // from incorrect usage of the pipe or if it originates from the
        // argument expressions.
        let (function, args, typ) = self.expr_typer.do_infer_call_with_known_fun(
            function,
            args,
            location,
            CallKind::Function,
        )?;
        Ok(TypedExpr::Call {
            location,
            typ,
            args,
            fun: Box::new(function),
        })
    }

    /// Attempt to infer a |> b(c) as b(a, c)
    fn infer_insert_pipe(
        &mut self,
        function: TypedExpr,
        mut arguments: Vec<CallArg<UntypedExpr>>,
        location: SrcSpan,
    ) -> Result<TypedExpr, Error> {
        arguments.insert(0, self.untyped_left_hand_value_variable_call_argument());
        // TODO: use `.with_unify_error_situation(UnifyErrorSituation::PipeTypeMismatch)`
        // This will require the typing of the arguments to be lifted up out of
        // the function below. If it is not we don't know if the error comes
        // from incorrect usage of the pipe or if it originates from the
        // argument expressions.
        let (fun, args, typ) = self.expr_typer.do_infer_call_with_known_fun(
            function,
            arguments,
            location,
            CallKind::Function,
        )?;
        Ok(TypedExpr::Call {
            location,
            typ,
            args,
            fun: Box::new(fun),
        })
    }

    /// Attempt to infer a |> b as b(a)
    fn infer_apply_pipe(&mut self, function: UntypedExpr) -> Result<TypedExpr, Error> {
        let function = Box::new(self.expr_typer.infer(function)?);
        let return_type = self.expr_typer.new_unbound_var();
        // Ensure that the function accepts one argument of the correct type
        unify(
            function.type_(),
            fn_(vec![self.argument_type.clone()], return_type.clone()),
        )
        .map_err(|e| {
            let is_pipe_mismatch = self.check_if_pipe_type_mismatch(&e);
            let error = convert_unify_error(e, function.location());
            if is_pipe_mismatch {
                error.with_unify_error_situation(UnifyErrorSituation::PipeTypeMismatch)
            } else {
                error
            }
        })?;

        Ok(TypedExpr::Call {
            location: function.location(),
            typ: return_type,
            fun: function,
            args: vec![self.typed_left_hand_value_variable_call_argument()],
        })
    }

    fn check_if_pipe_type_mismatch(&mut self, error: &UnifyError) -> bool {
        let types = match error {
            UnifyError::CouldNotUnify {
                expected, given, ..
            } => (expected.as_ref(), given.as_ref()),
            _ => return false,
        };

        match types {
            (Type::Fn { args: a, .. }, Type::Fn { args: b, .. }) if a.len() == b.len() => {
                match (a.first(), b.first()) {
                    (Some(a), Some(b)) => unify(a.clone(), b.clone()).is_err(),
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn warn_if_is_todo_or_panic(
        &self,
        call: &UntypedExpr,
        first_location: SrcSpan,
        previous_expression_location: Option<SrcSpan>,
    ) {
        let call_todo_or_panic = match call {
            UntypedExpr::Todo { .. } => Some(TodoOrPanic::Todo),
            UntypedExpr::Call { fun, .. } if fun.is_todo() => Some(TodoOrPanic::Todo),
            UntypedExpr::Panic { .. } => Some(TodoOrPanic::Panic),
            UntypedExpr::Call { fun, .. } if fun.is_panic() => Some(TodoOrPanic::Todo),
            _ => None,
        };

        if let Some(kind) = call_todo_or_panic {
            let args_location = if let Some(previous) = previous_expression_location {
                Some(SrcSpan::new(first_location.start, previous.end))
            } else {
                Some(first_location)
            };

            self.expr_typer
                .environment
                .warnings
                .emit(Warning::TodoOrPanicUsedAsFunction {
                    kind,
                    location: call.location(),
                    args_location,
                    args: 1,
                })
        }
    }
}
