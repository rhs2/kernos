//! Abstract syntax of a parsed policy.

/// A parsed policy: the optional `policy "name"` header and its rules in source
/// order. Rule indices in decision ids (`name@version#index`) are positions in
/// `rules`, 0-based.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// The name declared by the header, when present.
    pub name: Option<String>,
    /// Rules in source order.
    pub rules: Vec<Rule>,
}

/// What a rule does when its condition holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleKind {
    /// `deny when ...`
    Deny,
    /// `allow when ...`
    Allow,
    /// `require approval when ...`
    RequireApproval,
}

/// One rule of a policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// Deny, allow or require approval.
    pub kind: RuleKind,
    /// The condition after `when`.
    pub condition: Expr,
    /// The `-> approver: ...` clause; only meaningful on approval rules.
    pub approval: Option<ApprovalClause>,
}

/// The `-> approver: X, sla: D, escalate_to: E` clause of an approval rule.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalClause {
    /// Who must approve.
    pub approver: ApproverExpr,
    /// The service-level deadline in seconds, when given.
    pub sla_seconds: Option<u64>,
    /// Where to escalate on SLA expiry, when given.
    pub escalate_to: Option<EscalateExpr>,
}

/// An approver expression as written in the policy.
#[derive(Debug, Clone, PartialEq)]
pub enum ApproverExpr {
    /// `role("finance_admin")`
    Role(String),
    /// `user("u-tom")`
    User(String),
    /// A context path such as `run.requested_by.manager`.
    Path(Vec<String>),
}

/// An escalation target as written in the policy.
#[derive(Debug, Clone, PartialEq)]
pub enum EscalateExpr {
    /// `reporting_line`: the manager of the current approver.
    ReportingLine,
    /// `role("...")`
    Role(String),
    /// `user("...")`
    User(String),
    /// A context path.
    Path(Vec<String>),
}

/// Binary operators of the expression grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `or`
    Or,
    /// `and`
    And,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `in`
    In,
    /// `+`
    Add,
    /// `-`
    Sub,
}

/// An expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A number literal.
    Number(f64),
    /// A string literal.
    Str(String),
    /// `true` or `false`.
    Bool(bool),
    /// `null`.
    Null,
    /// A list literal.
    List(Vec<Expr>),
    /// A context path such as `action.amount`.
    Path(Vec<String>),
    /// A function call such as `action.touches_path("infra/**")`.
    Call {
        /// The callee path.
        path: Vec<String>,
        /// The arguments.
        args: Vec<Expr>,
    },
    /// Unary minus.
    Neg(Box<Expr>),
    /// `not`.
    Not(Box<Expr>),
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
}
