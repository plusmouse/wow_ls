//Copyright (C) 2025-  plusmouse and other contributors
//
//This program is free software: you can redistribute it and/or modify
//it under the terms of the GNU General Public License as published by
//the Free Software Foundation, either version 3 of the License, or
//(at your option) any later version.
//
//This program is distributed in the hope that it will be useful,
//but WITHOUT ANY WARRANTY; without even the implied warranty of
//MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//GNU General Public License for more details.
//
//You should have received a copy of the GNU General Public License
//along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Walking expressions, and the types operators give

use super::*;

impl<'a> Walker<'a> {
    /// The values an expression list produces, following Lua's multiple-value
    /// rules: a call or `...` at the end expands to all its values, anywhere
    /// else it's cut to one
    pub(super) fn expression_values(&mut self, expressions: &[Expression]) -> Values {
        let mut values = Values::none();
        for (i, expression) in expressions.iter().enumerate() {
            let last = i + 1 == expressions.len();
            match expression {
                Expression::FunctionCall(call) if last => {
                    let results = self.function_call(call);
                    values.values.extend(results.values);
                    values.rest = results.rest;
                }
                Expression::VarArgs(_) if last => match self.vararg_values() {
                    Some(arguments) => {
                        values.values.extend(arguments.values);
                        values.rest = arguments.rest;
                    }
                    None => values.rest = Type::UNKNOWN,
                },
                _ => values.values.push(self.expression(expression)),
            }
        }
        values
    }

    /// Types for `count` targets, with nil for any missing values
    pub(super) fn expression_list(&mut self, expressions: &[Expression], count: usize) -> Vec<Type> {
        let values = self.expression_values(expressions);
        (0..count).map(|i| values.get(i)).collect()
    }

    pub(super) fn expression(&mut self, expression: &Expression) -> Type {
        match expression {
            Expression::Literal(l) if l.get_string().is_some() => Type::string_literal(&l.get_string().unwrap_or_default()),
            Expression::Literal(l) => Type::of(if l.get_string().is_some() {
                TypeSet::STRING
            } else if l.get_number().is_some() {
                TypeSet::NUMBER
            } else if l.is_nil() {
                TypeSet::NIL
            } else if let Some(b) = l.get_bool() {
                if b.trim() == "true" { TypeSet::TRUE } else { TypeSet::FALSE }
            } else {
                TypeSet::UNKNOWN
            }),
            Expression::Function(def) => Type::function(self.function_body(def, None)),
            Expression::TableConstructor(t) => self.table_constructor(t.syntax()),
            // `f()` in the middle of an expression only gives its first value
            Expression::FunctionCall(call) => self.function_call(call).first(),
            Expression::Identifier(identifier) => match (identifier.simple_name(), self.global_table_access(identifier.syntax())) {
                (Some(name), _) => self.read(&name),
                // `_G.name`: the global
                (None, Some((token, name))) => {
                    self.identifier_parts(identifier.syntax());
                    let id = self.resolve_global(&name);
                    self.read_binding(&token, id)
                }
                (None, None) => match field_access(identifier.syntax()) {
                    // `a.b`: field `b` of whatever `a` is
                    Some((base, field)) => {
                        let base = self.expression(&base);
                        let types = self.field_read(base, field.text());
                        let types = self.narrowed_field(expression).unwrap_or(types);
                        self.field_reference(identifier.syntax(), &field, base, types, false);
                        self.check_field(base, &field, field.text());
                        types
                    }
                    None => match index_access(identifier.syntax()) {
                        // `t[k]`
                        Some((base, key)) => {
                            let base = self.expression(&base);
                            match self.index_key(&key) {
                                Key::Name(name, token) => {
                                    let types = self.field_read(base, &name);
                                    if let Some(token) = token {
                                        self.field_reference(identifier.syntax(), &token, base, types, false);
                                        self.check_field(base, &token, &name);
                                    }
                                    types
                                }
                                Key::Other(key) => {
                                    let types = self.index_read(base, key);
                                    // Read other than to call it, so any
                                    // function it gives may be called anywhere
                                    let called = identifier.syntax().parent().and_then(FunctionCall::cast)
                                        .and_then(|call| call.callee())
                                        .is_some_and(|callee| callee.syntax() == identifier.syntax());
                                    if !called {
                                        self.escape(types);
                                    }
                                    types
                                }
                            }
                        }
                        None => {
                            self.identifier_parts(identifier.syntax());
                            Type::UNKNOWN
                        }
                    },
                },
            },
            Expression::GroupedExpression(g) => match g.get_expression() {
                Some(e) => self.expression(&e),
                None => Type::UNKNOWN,
            },
            Expression::UnaryExpression(u) => {
                if let Some(operand) = u.operand() {
                    let types = self.expression(&operand);
                    self.check_operand(u.kind(), types, u.syntax().text_range());
                }
                Type::of(match u.kind() {
                    Operator::Not => TypeSet::BOOLEAN,
                    _ => TypeSet::NUMBER,
                })
            }
            Expression::BinaryExpression(b) => self.binary_expression(b),
            Expression::VarArgs(_) => self.vararg_values().map_or(Type::UNKNOWN, |arguments| arguments.first()),
        }
    }

    pub(super) fn function_call(&mut self, call: &FunctionCall) -> Values {
        let values = self.call(call);
        // The function may have changed any table's fields
        if !self.calls_library_global(call, "type") {
            self.forget_fields();
        }
        values
    }
}

impl<'a> Walker<'a> {
    pub(super) fn binary_expression(&mut self, b: &BinaryExpression) -> Type {
        match self.binary_tree(b) {
            Some(tree) => self.operation(&tree),
            None => {
                // Syntax error somewhere inside: still read what's there
                for part in b.flatten() {
                    if let BinaryPart::Operand(e) = part {
                        self.expression(&e);
                    }
                }
                Type::UNKNOWN
            }
        }
    }

    pub(super) fn binary_tree(&self, b: &BinaryExpression) -> Option<Operation> {
        let mut operands = Vec::new();
        let mut operators = Vec::new();
        for part in b.flatten() {
            match part {
                BinaryPart::Operand(e) => operands.push(e),
                BinaryPart::Operator(op) => operators.push(op),
            }
        }
        if operands.len() != operators.len() + 1 {
            return None
        }
        let mut position = 0;
        operation_tree(&mut operands.into_iter(), &operators, &mut position, 0)
    }

    /// The right of an `and` runs only if the left is true, and the right of
    /// an `or` only if it's false, so each sees what that proves. Either might
    /// not run at all
    pub(super) fn operation(&mut self, operation: &Operation) -> Type {
        match operation {
            Operation::Operand(e) => self.expression(e),
            Operation::Binary(op @ (Operator::And | Operator::Or), left, right) => {
                let left_type = self.operation(left);
                let proved = self.operation_narrowing(left);
                let proved = if *op == Operator::And { proved.when_true } else { proved.when_false };
                self.short_circuit_depth += 1;
                let right_type = self.with_narrowing(&proved, |w| w.operation(right));
                self.short_circuit_depth -= 1;
                combine(*op, left_type, right_type)
            }
            Operation::Binary(op, left_operation, right_operation) => {
                let left = self.operation(left_operation);
                let right = self.operation(right_operation);
                let range = left_operation.range().cover(right_operation.range());
                self.check_operands(*op, left, right, range);
                combine(*op, left, right)
            }
        }
    }
}

pub(super) fn precedence(op: Operator) -> u8 {
    match op {
        Operator::Or => 1,
        Operator::And => 2,
        Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals
            | Operator::GreaterThanOrEquals | Operator::NotEquals | Operator::Equals => 3,
        Operator::Concatenate => 4,
        Operator::Add | Operator::Subtract => 5,
        Operator::Multiply | Operator::Divide | Operator::Modulo => 6,
        Operator::Hat => 7,
        _ => 0,
    }
}

pub(super) fn combine(op: Operator, left: Type, right: Type) -> Type {
    match op {
        // The right side only runs if the left can be falsy (`or`) or truthy
        // (`and`). A left side not known yet may be, so `x or default` is the
        // default until it is: `t.list = t.list or {}` gives a table, where
        // nothing would go round to the read and stay nothing for good
        Operator::Or if left.set.falsy() == TypeSet::NEVER && left.set != TypeSet::NEVER => left,
        Operator::Or => left.truthy() | right,
        Operator::And if left.set.truthy() == TypeSet::NEVER => left,
        Operator::And => left.falsy() | right,
        Operator::Concatenate => Type::of(TypeSet::STRING),
        Operator::LessThan | Operator::GreaterThan | Operator::LessThanOrEquals
            | Operator::GreaterThanOrEquals | Operator::NotEquals | Operator::Equals => Type::of(TypeSet::BOOLEAN),
        // Ignores metatables (__add and friends)
        _ => Type::of(TypeSet::NUMBER),
    }
}
