use std::collections::HashMap;
use std::fmt;

use rowan::GreenNode;
use crate::ast::*;
use crate::syntax::SyntaxNode;

#[derive(Debug, Clone, PartialEq)]
pub enum ValueType {
    Nil,
    Boolean,
    Number,
    String,
    Function { params: Vec<ValueType>, returns: Vec<ValueType> },
    Table { fields: HashMap<std::string::String, ValueType> },
    Union(Vec<ValueType>),
    Unknown,
}

impl fmt::Display for ValueType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueType::Nil => write!(f, "nil"),
            ValueType::Boolean => write!(f, "boolean"),
            ValueType::Number => write!(f, "number"),
            ValueType::String => write!(f, "string"),
            ValueType::Unknown => write!(f, "unknown"),
            ValueType::Function { params, returns } => {
                write!(f, "function(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", p)?;
                }
                write!(f, ")")?;
                if !returns.is_empty() {
                    write!(f, " -> ")?;
                    for (i, r) in returns.iter().enumerate() {
                        if i > 0 { write!(f, ", ")?; }
                        write!(f, "{}", r)?;
                    }
                }
                Ok(())
            }
            ValueType::Table { fields } => {
                write!(f, "table{{")?;
                for (i, (k, v)) in fields.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, "}}")
            }
            ValueType::Union(types) => {
                for (i, t) in types.iter().enumerate() {
                    if i > 0 { write!(f, " | ")?; }
                    write!(f, "{}", t)?;
                }
                Ok(())
            }
        }
    }
}

struct Scope {
    parent: Option<usize>,
    locals: HashMap<std::string::String, ValueType>,
}

pub struct TypeEnvironment {
    scopes: Vec<Scope>,
    current_scope: usize,
    globals: HashMap<std::string::String, ValueType>,
}

impl TypeEnvironment {
    fn new() -> Self {
        TypeEnvironment {
            scopes: vec![Scope { parent: None, locals: HashMap::new() }],
            current_scope: 0,
            globals: HashMap::new(),
        }
    }

    fn push_scope(&mut self) {
        let new_idx = self.scopes.len();
        self.scopes.push(Scope {
            parent: Some(self.current_scope),
            locals: HashMap::new(),
        });
        self.current_scope = new_idx;
    }

    fn pop_scope(&mut self) {
        if let Some(parent) = self.scopes[self.current_scope].parent {
            self.current_scope = parent;
        }
    }

    fn set_local(&mut self, name: std::string::String, typ: ValueType) {
        self.scopes[self.current_scope].locals.insert(name, typ);
    }

    fn lookup(&self, name: &str) -> ValueType {
        let mut scope_idx = self.current_scope;
        loop {
            if let Some(t) = self.scopes[scope_idx].locals.get(name) {
                return t.clone();
            }
            match self.scopes[scope_idx].parent {
                Some(p) => scope_idx = p,
                None => break,
            }
        }
        if let Some(t) = self.globals.get(name) {
            return t.clone();
        }
        ValueType::Unknown
    }

    fn set_global(&mut self, name: std::string::String, typ: ValueType) {
        self.globals.insert(name, typ);
    }

    fn update_in_scope_or_global(&mut self, name: &str, typ: ValueType) {
        let mut scope_idx = self.current_scope;
        loop {
            if self.scopes[scope_idx].locals.contains_key(name) {
                self.scopes[scope_idx].locals.insert(name.to_string(), typ);
                return;
            }
            match self.scopes[scope_idx].parent {
                Some(p) => scope_idx = p,
                None => break,
            }
        }
        self.globals.insert(name.to_string(), typ);
    }

    fn update_table_field(&mut self, table_name: &str, field_name: &str, typ: ValueType) {
        let current = self.lookup(table_name);
        let mut fields = match current {
            ValueType::Table { fields } => fields,
            _ => HashMap::new(),
        };
        fields.insert(field_name.to_string(), typ);
        let new_type = ValueType::Table { fields };
        self.update_in_scope_or_global(table_name, new_type);
    }
}

fn get_expression_type(expr: &Expression, env: &TypeEnvironment) -> ValueType {
    match expr {
        Expression::Literal(l) => {
            if l.get_string().is_some() {
                ValueType::String
            } else if l.get_bool().is_some() {
                ValueType::Boolean
            } else if l.get_number().is_some() {
                ValueType::Number
            } else if l.is_nil() {
                ValueType::Nil
            } else {
                ValueType::Unknown
            }
        }
        Expression::Identifier(id) => {
            let names = id.names();
            if names.len() == 1 {
                env.lookup(&names[0])
            } else if names.len() >= 2 {
                let base = env.lookup(&names[0]);
                if let ValueType::Table { ref fields } = base {
                    fields.get(&names[1]).cloned().unwrap_or(ValueType::Unknown)
                } else {
                    ValueType::Unknown
                }
            } else {
                ValueType::Unknown
            }
        }
        Expression::BinaryExpression(bin) => {
            match bin.kind() {
                Operator::Add | Operator::Subtract | Operator::Multiply |
                Operator::Divide | Operator::Modulo | Operator::Hat => ValueType::Number,
                Operator::Concatenate => ValueType::String,
                Operator::LessThan | Operator::GreaterThan |
                Operator::LessThanOrEquals | Operator::GreaterThanOrEquals |
                Operator::Equals | Operator::NotEquals => ValueType::Boolean,
                Operator::And | Operator::Or => {
                    let terms = bin.get_terms();
                    if terms.len() == 2 {
                        let left = get_expression_type(&terms[0], env);
                        let right = get_expression_type(&terms[1], env);
                        if left == right {
                            left
                        } else {
                            ValueType::Union(vec![left, right])
                        }
                    } else {
                        ValueType::Unknown
                    }
                }
                _ => ValueType::Unknown,
            }
        }
        Expression::UnaryExpression(un) => {
            match un.kind() {
                Operator::Not => ValueType::Boolean,
                Operator::Subtract => ValueType::Number,
                Operator::ArrayLength => ValueType::Number,
                _ => ValueType::Unknown,
            }
        }
        Expression::GroupedExpression(g) => {
            match g.get_expression() {
                Some(inner) => get_expression_type(&inner, env),
                None => ValueType::Unknown,
            }
        }
        Expression::Function(func) => {
            let params: Vec<ValueType> = match func.params() {
                Some(pl) => pl.parameters().iter().map(|_| ValueType::Unknown).collect(),
                None => vec![],
            };
            let returns = match func.block() {
                Some(block) => infer_return_types(&block, env),
                None => vec![],
            };
            ValueType::Function { params, returns }
        }
        Expression::TableConstructor(tc) => {
            let mut fields = HashMap::new();
            if let Some(fl) = tc.field_list() {
                for field in fl.fields() {
                    if let Some(name) = field.name() {
                        let typ = match field.expression() {
                            Some(expr) => get_expression_type(&expr, env),
                            None => ValueType::Unknown,
                        };
                        fields.insert(name, typ);
                    }
                }
            }
            ValueType::Table { fields }
        }
    }
}

fn infer_return_types(block: &Block, env: &TypeEnvironment) -> Vec<ValueType> {
    let mut return_types: Vec<ValueType> = Vec::new();
    for stmt in block.statements() {
        if let Statement::Return(ret) = stmt {
            if let Some(exprs) = ret.expression_list() {
                let types: Vec<ValueType> = exprs.expressions().iter()
                    .map(|e| get_expression_type(e, env))
                    .collect();
                if types.len() == 1 {
                    let t = types.into_iter().next().unwrap();
                    if !return_types.contains(&t) {
                        return_types.push(t);
                    }
                } else if !types.is_empty() {
                    // Multiple return values - just take the first for now
                    let t = types.into_iter().next().unwrap();
                    if !return_types.contains(&t) {
                        return_types.push(t);
                    }
                }
            } else {
                if !return_types.contains(&ValueType::Nil) {
                    return_types.push(ValueType::Nil);
                }
            }
        }
    }
    return_types
}

fn process_block(block: &Block, env: &mut TypeEnvironment) {
    for stmt in block.statements() {
        process_statement(&stmt, env);
    }
}

fn process_statement(stmt: &Statement, env: &mut TypeEnvironment) {
    match stmt {
        Statement::LocalAssign(a) => {
            if let Some(name_list) = a.name_list() {
                let names = name_list.names();
                let exprs = a.expression_list()
                    .map(|el| el.expressions())
                    .unwrap_or_default();
                for (i, name) in names.iter().enumerate() {
                    let typ = if i < exprs.len() {
                        get_expression_type(&exprs[i], env)
                    } else {
                        ValueType::Nil
                    };
                    env.set_local(name.clone(), typ);
                }
            }
        }
        Statement::Assign(a) => {
            if let Some(var_list) = a.variable_list() {
                let exprs = a.expression_list()
                    .map(|el| el.expressions())
                    .unwrap_or_default();
                for (i, id) in var_list.identifiers().iter().enumerate() {
                    let typ = if i < exprs.len() {
                        get_expression_type(&exprs[i], env)
                    } else {
                        ValueType::Nil
                    };
                    let names = id.names();
                    if names.len() == 1 {
                        env.update_in_scope_or_global(&names[0], typ);
                    } else if names.len() >= 2 {
                        env.update_table_field(&names[0], &names[1], typ);
                    }
                }
            }
        }
        Statement::FunctionDefinition(func) => {
            let params: Vec<ValueType> = match func.params() {
                Some(pl) => pl.parameters().iter().map(|_| ValueType::Unknown).collect(),
                None => vec![],
            };
            let returns = match func.block() {
                Some(block) => {
                    env.push_scope();
                    if let Some(pl) = func.params() {
                        for p in pl.parameters() {
                            env.set_local(p, ValueType::Unknown);
                        }
                    }
                    let ret = infer_return_types(&block, env);
                    process_block(&block, env);
                    env.pop_scope();
                    ret
                }
                None => vec![],
            };
            let func_type = ValueType::Function { params, returns };
            if let Some(id) = func.identifier() {
                let names = id.names();
                if names.len() == 1 {
                    if func.is_local() {
                        env.set_local(names[0].clone(), func_type);
                    } else {
                        env.update_in_scope_or_global(&names[0], func_type);
                    }
                } else if names.len() >= 2 {
                    env.update_table_field(&names[0], &names[1], func_type);
                }
            }
        }
        Statement::Do(d) => {
            if let Some(block) = d.block() {
                env.push_scope();
                process_block(&block, env);
                env.pop_scope();
            }
        }
        Statement::While(w) => {
            if let Some(block) = w.block() {
                env.push_scope();
                process_block(&block, env);
                env.pop_scope();
            }
        }
        Statement::Repeat(r) => {
            if let Some(block) = r.block() {
                env.push_scope();
                process_block(&block, env);
                env.pop_scope();
            }
        }
        Statement::ForCountLoop(f) => {
            if let Some(block) = f.block() {
                env.push_scope();
                if let Some(name) = f.name() {
                    env.set_local(name, ValueType::Number);
                }
                process_block(&block, env);
                env.pop_scope();
            }
        }
        Statement::ForInLoop(f) => {
            if let Some(block) = f.block() {
                env.push_scope();
                if let Some(nl) = f.name_list() {
                    for name in nl.names() {
                        env.set_local(name, ValueType::Unknown);
                    }
                }
                process_block(&block, env);
                env.pop_scope();
            }
        }
        Statement::If(if_chain) => {
            for branch in if_chain.if_branches() {
                if let Some(block) = branch.block() {
                    env.push_scope();
                    process_block(&block, env);
                    env.pop_scope();
                }
            }
            if let Some(else_branch) = if_chain.else_branch() {
                if let Some(block) = else_branch.block() {
                    env.push_scope();
                    process_block(&block, env);
                    env.pop_scope();
                }
            }
        }
        Statement::Return(_) | Statement::FunctionCall(_) => {}
    }
}

pub fn get_types(green: GreenNode, _filename: &str) -> TypeEnvironment {
    let root = SyntaxNode::new_root(green);
    let block = Block::cast(root).expect("everything starts with a block");
    let mut env = TypeEnvironment::new();
    process_block(&block, &mut env);
    env
}

pub fn print_types(env: &TypeEnvironment) {
    println!("=== Globals ===");
    let mut globals: Vec<_> = env.globals.iter().collect();
    globals.sort_by_key(|(k, _)| k.clone());
    for (name, typ) in &globals {
        println!("  {} : {}", name, typ);
    }
    println!("=== Top-level locals ===");
    let mut locals: Vec<_> = env.scopes[0].locals.iter().collect();
    locals.sort_by_key(|(k, _)| k.clone());
    for (name, typ) in &locals {
        println!("  {} : {}", name, typ);
    }
}
