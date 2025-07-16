use crate::syntax::SyntaxNode;
use crate::syntax::SyntaxKind;

pub trait AstNode {
    fn cast(node: SyntaxNode) -> Option<Self>
    where
        Self: Sized;

    fn syntax(&self) -> &SyntaxNode;
}

pub enum Statement {
    Assign(Assign),
    LocalAssign(LocalAssign),
    FunctionCall(FunctionCall),
    Do(DoGroup),
    While(WhileGroup),
    Repeat(RepeatGroup),
    If(IfGroup),
    ForCountLoop(ForCountLoop),
    ForInLoop(ForInLoop),
    FunctionDefinition(FunctionDefinition),
}

impl AstNode for Statement {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::Assign => Some(Self::Assign(Assign{node})),
            SyntaxKind::LocalAssign => Some(Self::LocalAssign(LocalAssign{node})),
            SyntaxKind::FunctionCall => Some(Self::FunctionCall(FunctionCall{node})),
            SyntaxKind::Do => Some(Self::Do(DoGroup{node})),
            SyntaxKind::While => Some(Self::While(WhileGroup{node})),
            SyntaxKind::Repeat => Some(Self::Repeat(RepeatGroup{node})),
            SyntaxKind::If => Some(Self::If(IfGroup{node})),
            SyntaxKind::ForCountLoop => Some(Self::ForCountLoop(ForCountLoop{node})),
            SyntaxKind::ForInLoop => Some(Self::ForInLoop(ForInLoop{node})),
            SyntaxKind::FunctionDefinition => Some(Self::FunctionDefinition(FunctionDefinition{node})),
            _ => None,
        }
    }
    fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Assign(x) => x.syntax(),
            Self::LocalAssign(x) => x.syntax(),
            Self::FunctionCall(x) => x.syntax(),
            Self::Do(x) => x.syntax(),
            Self::While(x) => x.syntax(),
            Self::Repeat(x) => x.syntax(),
            Self::If(x) => x.syntax(),
            Self::ForCountLoop(x) => x.syntax(),
            Self::ForInLoop(x) => x.syntax(),
            Self::FunctionDefinition(x) => x.syntax(),
        }
    }
}

struct FunctionDefinition {
    node: SyntaxNode
}
impl AstNode for FunctionDefinition {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::FunctionDefinition => Some(Self{node}),
            _ => None,
        }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.node
    }
}
impl FunctionDefinition {
    pub fn is_local(&self) -> bool {
        match self.node.first_child_or_token_by_kind(&|k| k == SyntaxKind::LocalKeyword) {
            Some(_) => true,
            _ => false,
        }
    }
    pub fn identifier(&self) -> Option<Identifier> {
        self.node.children().find_map(Identifier::cast)
    }
    pub fn params(&self) -> Option<ParameterList> {
        self.node.children().find_map(ParameterList::cast)
    }
    pub fn block(&self) -> Option<Block> {
        self.node.children().find_map(Block::cast)
    }
}

struct ParameterList {
    node: SyntaxNode
}

impl AstNode for ParameterList {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::ParameterList => Some(Self{node}),
            _ => None,
        }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.node
    }
}

struct Identifier {
    node: SyntaxNode
}

impl AstNode for Identifier {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::Identifier => Some(Self{node}),
            _ => None,
        }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.node
    }
}

struct Block {
    node: SyntaxNode
}

impl AstNode for Block {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::Block => Some(Self{node}),
            _ => None,
        }
    }
    fn syntax(&self) -> &SyntaxNode {
        &self.node
    }
}

impl Block {
    pub fn statements(&self) -> Vec<Statement> {
        self.node.children().filter_map(Statement::cast).collect()
    }
}
