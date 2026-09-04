//! Recursive-descent parser for the grammar in 04-POLICY.

use crate::ast::{
    ApprovalClause, ApproverExpr, BinaryOp, EscalateExpr, Expr, Policy, Rule, RuleKind,
};
use crate::error::ParseError;
use crate::lexer::{is_keyword, lex, Token, TokenKind};

/// The callee paths the evaluator understands. Any other call is rejected at
/// parse time so a typo cannot silently evaluate to `null` in production.
pub const KNOWN_FUNCTIONS: &[&[&str]] = &[
    &["action", "touches_path"],
    &["action", "touches_data_class"],
    &["run", "remit", "grants"],
];

/// Parses policy text into a [`Policy`]. Every error carries line and column.
pub fn parse(source: &str) -> Result<Policy, ParseError> {
    let tokens = lex(source)?;
    let mut parser = Parser { tokens, pos: 0 };
    parser.policy()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        // The lexer always appends Eof, so the last token is a safe fallback.
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| &self.tokens[self.tokens.len() - 1])
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        token
    }

    fn error<T>(&self, message: impl Into<String>) -> Result<T, ParseError> {
        let token = self.peek();
        Err(ParseError::new(token.line, token.column, message))
    }

    fn is_ident(&self, word: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(w) if w == word)
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), ParseError> {
        if self.is_ident(word) {
            self.advance();
            Ok(())
        } else {
            self.error(format!(
                "expected '{word}', found {}",
                describe(&self.peek().kind)
            ))
        }
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<(), ParseError> {
        if self.peek().kind == kind {
            self.advance();
            Ok(())
        } else {
            self.error(format!(
                "expected {what}, found {}",
                describe(&self.peek().kind)
            ))
        }
    }

    fn expect_string(&mut self) -> Result<String, ParseError> {
        match &self.peek().kind {
            TokenKind::Str(s) => {
                let s = s.clone();
                self.advance();
                Ok(s)
            }
            other => self.error(format!("expected a string, found {}", describe(other))),
        }
    }

    fn policy(&mut self) -> Result<Policy, ParseError> {
        let mut name = None;
        if self.is_ident("policy") {
            self.advance();
            name = Some(self.expect_string()?);
        }
        let mut rules = Vec::new();
        loop {
            match &self.peek().kind {
                TokenKind::Eof => break,
                TokenKind::Ident(w) if w == "deny" || w == "allow" || w == "require" => {
                    rules.push(self.rule()?);
                }
                TokenKind::Ident(w) if w == "policy" => {
                    return self.error("the policy header must come first and appear once")
                }
                other => {
                    return self.error(format!(
                        "expected 'deny', 'allow' or 'require approval', found {}",
                        describe(other)
                    ))
                }
            }
        }
        Ok(Policy { name, rules })
    }

    fn rule(&mut self) -> Result<Rule, ParseError> {
        let keyword = self.advance();
        let kind = match &keyword.kind {
            TokenKind::Ident(w) if w == "deny" => RuleKind::Deny,
            TokenKind::Ident(w) if w == "allow" => RuleKind::Allow,
            TokenKind::Ident(w) if w == "require" => {
                self.expect_keyword("approval")?;
                RuleKind::RequireApproval
            }
            other => return self.error(format!("expected a rule, found {}", describe(other))),
        };
        self.expect_keyword("when")?;
        let condition = self.expr()?;
        let mut approval = None;
        if self.peek().kind == TokenKind::Arrow {
            let arrow = self.advance();
            if kind != RuleKind::RequireApproval {
                return Err(ParseError::new(
                    arrow.line,
                    arrow.column,
                    "an approval clause is only allowed on 'require approval' rules",
                ));
            }
            approval = Some(self.approval_clause()?);
        }
        Ok(Rule {
            kind,
            condition,
            approval,
        })
    }

    fn approval_clause(&mut self) -> Result<ApprovalClause, ParseError> {
        self.expect_keyword("approver")?;
        self.expect(TokenKind::Colon, "':'")?;
        let approver = self.approver()?;
        let mut sla_seconds = None;
        let mut escalate_to = None;
        while self.peek().kind == TokenKind::Comma {
            self.advance();
            if self.is_ident("sla") {
                if sla_seconds.is_some() {
                    return self.error("'sla' given twice");
                }
                self.advance();
                self.expect(TokenKind::Colon, "':'")?;
                match &self.peek().kind {
                    TokenKind::Duration(seconds) => {
                        sla_seconds = Some(*seconds);
                        self.advance();
                    }
                    other => {
                        return self.error(format!(
                            "expected a duration such as 4h, found {}",
                            describe(other)
                        ))
                    }
                }
            } else if self.is_ident("escalate_to") {
                if escalate_to.is_some() {
                    return self.error("'escalate_to' given twice");
                }
                self.advance();
                self.expect(TokenKind::Colon, "':'")?;
                escalate_to = Some(self.escalate()?);
            } else {
                return self.error(format!(
                    "expected 'sla' or 'escalate_to', found {}",
                    describe(&self.peek().kind)
                ));
            }
        }
        Ok(ApprovalClause {
            approver,
            sla_seconds,
            escalate_to,
        })
    }

    fn approver(&mut self) -> Result<ApproverExpr, ParseError> {
        if self.is_ident("role") {
            self.advance();
            Ok(ApproverExpr::Role(self.quoted_argument()?))
        } else if self.is_ident("user") {
            self.advance();
            Ok(ApproverExpr::User(self.quoted_argument()?))
        } else {
            Ok(ApproverExpr::Path(self.path()?))
        }
    }

    fn escalate(&mut self) -> Result<EscalateExpr, ParseError> {
        if self.is_ident("reporting_line") {
            self.advance();
            Ok(EscalateExpr::ReportingLine)
        } else if self.is_ident("role") {
            self.advance();
            Ok(EscalateExpr::Role(self.quoted_argument()?))
        } else if self.is_ident("user") {
            self.advance();
            Ok(EscalateExpr::User(self.quoted_argument()?))
        } else {
            Ok(EscalateExpr::Path(self.path()?))
        }
    }

    fn quoted_argument(&mut self) -> Result<String, ParseError> {
        self.expect(TokenKind::LParen, "'('")?;
        let value = self.expect_string()?;
        self.expect(TokenKind::RParen, "')'")?;
        Ok(value)
    }

    fn path(&mut self) -> Result<Vec<String>, ParseError> {
        let mut segments = vec![self.ident_segment()?];
        while self.peek().kind == TokenKind::Dot {
            self.advance();
            segments.push(self.ident_segment()?);
        }
        Ok(segments)
    }

    fn ident_segment(&mut self) -> Result<String, ParseError> {
        match &self.peek().kind {
            TokenKind::Ident(w) if !is_keyword(w) => {
                let w = w.clone();
                self.advance();
                Ok(w)
            }
            TokenKind::Ident(w) => self.error(format!(
                "'{w}' is a reserved word and cannot be a path segment"
            )),
            other => self.error(format!("expected an identifier, found {}", describe(other))),
        }
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and()?;
        while self.is_ident("or") {
            self.advance();
            let right = self.and()?;
            left = binary(BinaryOp::Or, left, right);
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.not()?;
        while self.is_ident("and") {
            self.advance();
            let right = self.not()?;
            left = binary(BinaryOp::And, left, right);
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, ParseError> {
        if self.is_ident("not") {
            self.advance();
            let inner = self.not()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.cmp()
    }

    fn cmp(&mut self) -> Result<Expr, ParseError> {
        let left = self.sum()?;
        let op = match &self.peek().kind {
            TokenKind::Eq => BinaryOp::Eq,
            TokenKind::Ne => BinaryOp::Ne,
            TokenKind::Lt => BinaryOp::Lt,
            TokenKind::Le => BinaryOp::Le,
            TokenKind::Gt => BinaryOp::Gt,
            TokenKind::Ge => BinaryOp::Ge,
            TokenKind::Ident(w) if w == "in" => BinaryOp::In,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.sum()?;
        Ok(binary(op, left, right))
    }

    fn sum(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.unary()?;
            left = binary(op, left, right);
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.peek().kind == TokenKind::Minus {
            self.advance();
            let inner = self.unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let token = self.peek().clone();
        match token.kind {
            TokenKind::Number(n) => {
                self.advance();
                Ok(Expr::Number(n))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(Expr::Str(s))
            }
            TokenKind::LParen => {
                self.advance();
                let inner = self.expr()?;
                self.expect(TokenKind::RParen, "')'")?;
                Ok(inner)
            }
            TokenKind::LBracket => {
                self.advance();
                let mut items = Vec::new();
                if self.peek().kind != TokenKind::RBracket {
                    loop {
                        items.push(self.expr()?);
                        if self.peek().kind == TokenKind::Comma {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(TokenKind::RBracket, "']'")?;
                Ok(Expr::List(items))
            }
            TokenKind::Ident(ref w) if w == "true" => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            TokenKind::Ident(ref w) if w == "false" => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            TokenKind::Ident(ref w) if w == "null" => {
                self.advance();
                Ok(Expr::Null)
            }
            TokenKind::Ident(_) => {
                let path = self.path()?;
                if self.peek().kind == TokenKind::LParen {
                    let known = KNOWN_FUNCTIONS
                        .iter()
                        .any(|f| f.len() == path.len() && f.iter().zip(&path).all(|(a, b)| a == b));
                    if !known {
                        return Err(ParseError::new(
                            token.line,
                            token.column,
                            format!("unknown function {}", path.join(".")),
                        ));
                    }
                    self.advance();
                    let mut args = Vec::new();
                    if self.peek().kind != TokenKind::RParen {
                        loop {
                            args.push(self.expr()?);
                            if self.peek().kind == TokenKind::Comma {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(TokenKind::RParen, "')'")?;
                    return Ok(Expr::Call { path, args });
                }
                Ok(Expr::Path(path))
            }
            ref other => self.error(format!("expected an expression, found {}", describe(other))),
        }
    }
}

fn binary(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::Binary {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

fn describe(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Ident(w) => format!("'{w}'"),
        TokenKind::Number(n) => format!("number {n}"),
        TokenKind::Str(s) => format!("string {s:?}"),
        TokenKind::Duration(s) => format!("duration of {s} seconds"),
        TokenKind::LParen => "'('".into(),
        TokenKind::RParen => "')'".into(),
        TokenKind::LBracket => "'['".into(),
        TokenKind::RBracket => "']'".into(),
        TokenKind::Comma => "','".into(),
        TokenKind::Dot => "'.'".into(),
        TokenKind::Colon => "':'".into(),
        TokenKind::Arrow => "'->'".into(),
        TokenKind::Eq => "'=='".into(),
        TokenKind::Ne => "'!='".into(),
        TokenKind::Lt => "'<'".into(),
        TokenKind::Le => "'<='".into(),
        TokenKind::Gt => "'>'".into(),
        TokenKind::Ge => "'>='".into(),
        TokenKind::Plus => "'+'".into(),
        TokenKind::Minus => "'-'".into(),
        TokenKind::Eof => "end of input".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
# finance-default v1
policy "finance-default"

require approval when
  action.kind == "payment.issue" and action.amount >= 5000
  -> approver: role("finance_admin"), sla: 4h, escalate_to: reporting_line

require approval when
  action.writes_to_system_of_record and run.remit.autonomy == "supervised"
  -> approver: run.requested_by.manager, sla: 24h

require approval when
  action.kind == "code.merge" and action.touches_path("infra/**")
  -> approver: role("platform_owner"), sla: 8h

deny when
  action.touches_data_class("personal") and not run.remit.grants("pii")

allow when
  action.kind == "invoice.read"
"#;

    #[test]
    fn parses_the_spec_example() {
        let policy = parse(EXAMPLE).expect("parse");
        assert_eq!(policy.name.as_deref(), Some("finance-default"));
        assert_eq!(policy.rules.len(), 5);
        assert_eq!(policy.rules[0].kind, RuleKind::RequireApproval);
        let clause = policy.rules[0].approval.as_ref().expect("clause");
        assert_eq!(clause.approver, ApproverExpr::Role("finance_admin".into()));
        assert_eq!(clause.sla_seconds, Some(14400));
        assert_eq!(clause.escalate_to, Some(EscalateExpr::ReportingLine));
        let clause = policy.rules[1].approval.as_ref().expect("clause");
        assert_eq!(
            clause.approver,
            ApproverExpr::Path(vec!["run".into(), "requested_by".into(), "manager".into()])
        );
        assert_eq!(clause.sla_seconds, Some(86400));
        assert_eq!(clause.escalate_to, None);
        assert_eq!(policy.rules[3].kind, RuleKind::Deny);
        assert_eq!(policy.rules[4].kind, RuleKind::Allow);
    }

    #[test]
    fn precedence_binds_and_before_or_and_not_tightest() {
        let policy = parse("allow when a or b and not c == 1").expect("parse");
        let Expr::Binary { op, right, .. } = &policy.rules[0].condition else {
            panic!("expected binary")
        };
        assert_eq!(*op, BinaryOp::Or);
        let Expr::Binary { op, right, .. } = right.as_ref() else {
            panic!("expected and")
        };
        assert_eq!(*op, BinaryOp::And);
        assert!(matches!(right.as_ref(), Expr::Not(_)));
    }

    #[test]
    fn arithmetic_and_lists() {
        let policy = parse("allow when action.amount + 5 - -2 in [1, 2, 3]").expect("parse");
        assert!(matches!(
            policy.rules[0].condition,
            Expr::Binary {
                op: BinaryOp::In,
                ..
            }
        ));
    }

    #[test]
    fn errors_report_line_and_column() {
        let err = parse("allow when action.kind ==\n  ").expect_err("incomplete");
        assert_eq!((err.line, err.column), (2, 3));
        let err = parse("deny when x -> approver: role(\"a\")").expect_err("clause on deny");
        assert_eq!((err.line, err.column), (1, 13));
        let err =
            parse("require approval when x\n -> approver: role(\"a\"), sla: 5").expect_err("sla");
        assert_eq!((err.line, err.column), (2, 31));
        let err = parse("allow when run.user == 1").expect_err("reserved");
        assert_eq!((err.line, err.column), (1, 16));
        let err = parse("allow when action.foo(1)").expect_err("unknown fn");
        assert_eq!((err.line, err.column), (1, 12));
        assert!(err.message.contains("unknown function"));
        let err = parse("allow when x\npolicy \"late\"").expect_err("late header");
        assert_eq!(err.line, 2);
    }

    #[test]
    fn empty_policy_is_valid() {
        let policy = parse("# nothing here\n").expect("parse");
        assert!(policy.rules.is_empty());
        assert!(policy.name.is_none());
    }
}
